//! The sound of a video: what its sound track holds, and its samples.
//!
//! AAC (phones, action cameras, most cameras today) is decoded with FilmCraft's clean-room
//! `filmcraft-aac`; uncompressed PCM (older cameras, some recorders) is read directly. Anything
//! else is named and reported as not playable: the picture plays without it.
//!
//! Samples come out as interleaved `f32` in -1..1 at the track's own rate, front channels first
//! (left, right, centre, low frequency, then the surrounds).

use std::sync::Arc;

use filmcraft_isobmff::{CodecConfig, Mp4File, PcmConfig, Track, TrackKind};

use crate::{Error, Inner, Result, guard};

/// Rates and channel counts outside these are not sound we play.
const MAX_RATE: u32 = 384_000;
const MAX_CHANNELS: usize = 8;
/// One compressed packet of sound larger than this is not one.
const MAX_PACKET: u32 = 1 << 20;
/// Frames of uncompressed sound read at a time.
const PCM_FRAMES: u64 = 4096;

/// How the sound is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioCodec {
    Aac,
    /// Uncompressed.
    Pcm,
    /// Anything else, by its sample-entry code (`ulaw`, `ac-3`, `.mp3`…): no decoder here.
    Other(String),
}

impl AudioCodec {
    /// The name people know it by.
    pub fn name(&self) -> &str {
        match self {
            AudioCodec::Aac => "AAC",
            AudioCodec::Pcm => "PCM",
            AudioCodec::Other(code) => code,
        }
    }
}

/// What a video's sound track declares, read from its headers alone.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    pub codec: AudioCodec,
    /// Samples a second, per channel.
    pub sample_rate: u32,
    pub channels: u16,
    /// Seconds.
    pub duration: f64,
    /// This build can decode it ([`crate::Video::audio_reader`] works).
    pub playable: bool,
}

/// A run of sound: interleaved samples in -1..1, and when its first one is heard.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioBlock {
    pub samples: Vec<f32>,
    /// Seconds from the start of the video.
    pub time: f64,
}

fn code_of(track: &Track) -> String {
    track.entries.first().map(|e| e.format.to_string_lossy()).unwrap_or_default().trim().to_string()
}

/// Is this PCM layout one [`pcm_samples`] reads?
fn pcm_readable(p: &PcmConfig) -> bool {
    if p.float { matches!(p.bits, 32 | 64) } else { matches!(p.bits, 8 | 16 | 24 | 32) }
}

fn in_range(rate: u32, channels: usize) -> bool {
    (1..=MAX_RATE).contains(&rate) && (1..=MAX_CHANNELS).contains(&channels)
}

/// The file's sound track (the first enabled one that has samples) and what it holds.
pub(crate) fn describe(file: &Mp4File) -> Option<(usize, AudioInfo)> {
    let usable = |t: &&Track| t.kind == TrackKind::Audio && !t.samples.is_empty();
    let index = file.tracks.iter().position(|t| usable(&t) && t.enabled).or_else(|| file.tracks.iter().position(|t| usable(&t)))?;
    let track = file.tracks.get(index)?;
    let declared = track.audio().map(|a| (a.sample_rate, a.channels)).unwrap_or((0.0, 0));
    let (codec, rate, channels, playable) = match track.codec() {
        Some(CodecConfig::Aac(config)) => match filmcraft_aac::Decoder::new(&config.asc) {
            Ok(decoder) => (AudioCodec::Aac, decoder.sample_rate(), decoder.channels(), true),
            Err(_) => (AudioCodec::Aac, config.sample_rate, declared.1 as usize, false),
        },
        Some(CodecConfig::Pcm(p)) => (AudioCodec::Pcm, p.sample_rate as u32, p.channels as usize, pcm_readable(p)),
        _ => (AudioCodec::Other(code_of(track)), declared.0 as u32, declared.1 as usize, false),
    };
    let duration = if track.timescale == 0 { 0.0 } else { track.duration as f64 / f64::from(track.timescale) };
    let info = AudioInfo {
        codec,
        sample_rate: rate,
        channels: u16::try_from(channels).unwrap_or(u16::MAX),
        duration,
        playable: playable && in_range(rate, channels),
    };
    Some((index, info))
}

/// Uncompressed sound → samples in -1..1. Bytes that don't make a whole sample are left out.
fn pcm_samples(data: &[u8], p: &PcmConfig) -> Vec<f32> {
    let be = p.big_endian;
    match (p.float, p.bits) {
        (false, 8) if p.signed => data.iter().map(|b| f32::from(*b as i8) / 128.0).collect(),
        (false, 8) => data.iter().map(|b| (f32::from(*b) - 128.0) / 128.0).collect(),
        (false, 16) => {
            data.as_chunks::<2>().0.iter().map(|b| f32::from(if be { i16::from_be_bytes(*b) } else { i16::from_le_bytes(*b) }) / 32_768.0).collect()
        }
        (false, 24) => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|b| {
                // the three bytes as the top of a 32-bit value: the sign comes with them
                let v = if be { i32::from_be_bytes([b[0], b[1], b[2], 0]) } else { i32::from_le_bytes([0, b[0], b[1], b[2]]) };
                (v >> 8) as f32 / 8_388_608.0
            })
            .collect(),
        (false, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| (if be { i32::from_be_bytes(*b) } else { i32::from_le_bytes(*b) }) as f32 / 2_147_483_648.0)
            .collect(),
        (true, 32) => data.as_chunks::<4>().0.iter().map(|b| sane(if be { f32::from_be_bytes(*b) } else { f32::from_le_bytes(*b) })).collect(),
        (true, 64) => {
            data.as_chunks::<8>().0.iter().map(|b| sane((if be { f64::from_be_bytes(*b) } else { f64::from_le_bytes(*b) }) as f32)).collect()
        }
        _ => Vec::new(),
    }
}

/// A sample a loudspeaker can be given: finite and within -1..1.
fn sane(v: f32) -> f32 {
    if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 }
}

/// Where each output channel comes from in what the AAC decoder gives (centre first, low
/// frequency last): front left and right first, as everything downstream expects.
fn aac_order(channels: usize) -> &'static [usize] {
    match channels {
        3 => &[1, 2, 0],
        4 => &[1, 2, 0, 3],
        5 => &[1, 2, 0, 3, 4],
        6 => &[1, 2, 0, 5, 3, 4],
        8 => &[1, 2, 0, 7, 5, 6, 3, 4],
        _ => &[],
    }
}

/// The decoder's channels, one after the other → `channels` interleaved.
fn interleave(planes: &[Vec<f32>], channels: usize) -> Vec<f32> {
    let frames = planes.iter().map(Vec::len).min().unwrap_or(0);
    let order = aac_order(planes.len().min(channels));
    let mut out = Vec::with_capacity(frames.saturating_mul(channels));
    for frame in 0..frames {
        for channel in 0..channels {
            let from = order.get(channel).copied().unwrap_or(channel);
            out.push(planes.get(from).and_then(|p| p.get(frame)).copied().map_or(0.0, sane));
        }
    }
    out
}

enum Decoder {
    /// The decoder, and the configuration it was made from (to start over after a failure).
    Aac(Box<filmcraft_aac::Decoder>, Vec<u8>),
    Pcm(PcmConfig),
}

fn aac(asc: &[u8]) -> Result<Box<filmcraft_aac::Decoder>> {
    filmcraft_aac::Decoder::new(asc).map(Box::new).map_err(|e| Error::Malformed(format!("the sound: {e}")))
}

/// Reads the sound of a [`crate::Video`] from its start, or from where [`AudioReader::seek`]
/// put it. Owns its decoder, so it can live on a worker thread.
pub struct AudioReader {
    video: Arc<Inner>,
    track: usize,
    decoder: Decoder,
    rate: u32,
    channels: usize,
    /// The next packet (or, for PCM, chunk) to read.
    next: usize,
    /// PCM: frames of chunk `next` already given.
    given: u64,
    /// After a seek: sound heard before this time (seconds) is decoded, not returned.
    from: Option<f64>,
}

impl AudioReader {
    pub(crate) fn new(video: Arc<Inner>) -> Result<Self> {
        let (track, info) = video.audio.clone().ok_or_else(|| Error::Unsupported("the video has no sound".into()))?;
        if !info.playable {
            return Err(Error::Unsupported(format!("no decoder for {} sound", info.codec.name())));
        }
        let t = video.file.tracks.get(track).ok_or_else(|| Error::Malformed("no sound track".into()))?;
        let decoder = match t.codec() {
            Some(CodecConfig::Aac(config)) => guard("starting the sound decoder", || Ok(Decoder::Aac(aac(&config.asc)?, config.asc.clone())))?,
            Some(CodecConfig::Pcm(p)) => Decoder::Pcm(*p),
            _ => return Err(Error::Unsupported(format!("no decoder for {} sound", info.codec.name()))),
        };
        Ok(AudioReader { rate: info.sample_rate, channels: usize::from(info.channels), video, track, decoder, next: 0, given: 0, from: None })
    }

    /// Samples a second, per channel.
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Channels in every block (interleaved).
    pub fn channels(&self) -> usize {
        self.channels
    }

    fn track(&self) -> Result<&Track> {
        self.video.file.tracks.get(self.track).ok_or_else(|| Error::Malformed("no sound track".into()))
    }

    /// Continue from the sound heard at `seconds` (clamped to the track).
    pub fn seek(&mut self, seconds: f64) -> Result<()> {
        let seconds = if seconds.is_finite() { seconds.max(0.0) } else { 0.0 };
        let t = self.track()?;
        let time = (seconds * f64::from(t.timescale)) as i64;
        let last = t.samples.len().saturating_sub(1);
        let at = t.sample_at_presentation_time(time).unwrap_or(last).min(last);
        // an AAC packet builds on the one before it: that one is decoded too, and not returned
        let (next, fresh) = match &self.decoder {
            Decoder::Aac(_, asc) => (at.saturating_sub(1), Some(guard("starting the sound decoder", || aac(asc))?)),
            Decoder::Pcm(_) => (at, None),
        };
        if let (Decoder::Aac(decoder, _), Some(fresh)) = (&mut self.decoder, fresh) {
            *decoder = fresh;
        }
        self.next = next;
        self.given = 0;
        self.from = Some(seconds);
        Ok(())
    }

    /// The next run of sound, or `None` after the last one.
    pub fn next_block(&mut self) -> Result<Option<AudioBlock>> {
        loop {
            let Some((mut samples, mut time)) = self.read()? else { return Ok(None) };
            let rate = f64::from(self.rate.max(1));
            let channels = self.channels.max(1);
            // sound from before the start of the video (an encoder's lead-in) or before the
            // seek is left out
            let start = self.from.unwrap_or(0.0).max(0.0);
            if time < start {
                let frames = samples.len() / channels;
                let drop = (((start - time) * rate).round() as usize).min(frames);
                samples.drain(..drop * channels);
                time += drop as f64 / rate;
            }
            if !samples.is_empty() {
                return Ok(Some(AudioBlock { samples, time }));
            }
        }
    }

    /// Decode the next packet or piece of a chunk → (interleaved samples, seconds).
    fn read(&mut self) -> Result<Option<(Vec<f32>, f64)>> {
        let video = self.video.clone();
        let t = video.file.tracks.get(self.track).ok_or_else(|| Error::Malformed("no sound track".into()))?;
        let Some(sample) = t.samples.get(self.next) else { return Ok(None) };
        let scale = f64::from(t.timescale.max(1));
        let time = t.presentation_pts(self.next).unwrap_or(sample.pts) as f64 / scale;
        let (rate, channels) = (f64::from(self.rate.max(1)), self.channels.max(1));
        match &mut self.decoder {
            Decoder::Aac(decoder, asc) => {
                if sample.size > MAX_PACKET {
                    return Err(Error::Limit("a packet of the sound".into()));
                }
                let index = self.next;
                self.next += 1;
                let data = guard("reading the sound", || Ok(video.file.read_sample(&*video.source, self.track, index)?))?;
                let decoded = guard("decoding the sound", || decoder.decode(&data).map_err(|e| Error::Malformed(e.to_string())));
                let samples = match decoded {
                    Ok(planes) => interleave(&planes, channels),
                    Err(_) => {
                        // a damaged packet is silence, and the decoder starts over after it
                        *decoder = guard("starting the sound decoder", || aac(asc))?;
                        vec![0.0; filmcraft_aac::FRAME_LEN * channels]
                    }
                };
                Ok(Some((samples, time)))
            }
            Decoder::Pcm(p) => {
                // a chunk can be the whole track: it is read a piece at a time
                let frame = u64::from(p.bytes_per_frame().max(1));
                let frames = u64::from(sample.size) / frame;
                let take = frames.saturating_sub(self.given).min(PCM_FRAMES);
                if take == 0 {
                    self.next += 1;
                    self.given = 0;
                    return Ok(Some((Vec::new(), time)));
                }
                let offset =
                    sample.offset.checked_add(self.given * frame).ok_or_else(|| Error::Malformed("the sound lies outside the file".into()))?;
                let mut data = vec![0u8; (take * frame) as usize];
                video.source.read_at(offset, &mut data).map_err(|e| Error::Io(e.to_string()))?;
                let time = time + self.given as f64 / rate;
                self.given += take;
                let mut samples = pcm_samples(&data, p);
                samples.truncate(samples.len() / channels * channels);
                Ok(Some((samples, time)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(bits: u16, float: bool, big_endian: bool, signed: bool) -> PcmConfig {
        PcmConfig { bits, float, big_endian, signed, channels: 1, sample_rate: 8000.0 }
    }

    #[test]
    fn uncompressed_sound_of_every_layout_reads_as_minus_one_to_one() {
        assert_eq!(pcm_samples(&[0, 128, 255], &pcm(8, false, true, false)), [-1.0, 0.0, 127.0 / 128.0]);
        assert_eq!(pcm_samples(&[0x80, 0, 0x40], &pcm(8, false, true, true)), [-1.0, 0.0, 0.5]);
        assert_eq!(pcm_samples(&[0x80, 0, 0x40, 0, 9], &pcm(16, false, true, true)), [-1.0, 0.5], "an odd byte is left out");
        assert_eq!(pcm_samples(&[0, 0x80, 0, 0x40], &pcm(16, false, false, true)), [-1.0, 0.5]);
        assert_eq!(pcm_samples(&[0x80, 0, 0, 0x40, 0, 0], &pcm(24, false, true, true)), [-1.0, 0.5]);
        assert_eq!(pcm_samples(&[0, 0, 0x80, 0, 0, 0x40], &pcm(24, false, false, true)), [-1.0, 0.5]);
        assert_eq!(pcm_samples(&[0x80, 0, 0, 0, 0x40, 0, 0, 0], &pcm(32, false, true, true)), [-1.0, 0.5]);
        assert_eq!(pcm_samples(&0.25f32.to_le_bytes(), &pcm(32, true, false, true)), [0.25]);
        assert_eq!(pcm_samples(&(-0.5f64).to_be_bytes(), &pcm(64, true, true, true)), [-0.5]);
        // values no loudspeaker should be given
        assert_eq!(pcm_samples(&f32::NAN.to_le_bytes(), &pcm(32, true, false, true)), [0.0]);
        assert_eq!(pcm_samples(&1e9f32.to_le_bytes(), &pcm(32, true, false, true)), [1.0]);
        // a layout that is not read
        assert!(pcm_samples(&[1, 2, 3, 4], &pcm(12, false, true, true)).is_empty());
        assert!(!pcm_readable(&pcm(12, false, true, true)) && pcm_readable(&pcm(24, false, true, true)) && !pcm_readable(&pcm(16, true, true, true)));
    }

    #[test]
    fn decoded_channels_are_interleaved_front_first() {
        assert_eq!(interleave(&[vec![0.1, 0.2], vec![0.3, 0.4]], 2), [0.1, 0.3, 0.2, 0.4]);
        // 5.1 from the decoder: centre, left, right, the surrounds, low frequency
        let planes: Vec<Vec<f32>> = (0..6).map(|c| vec![c as f32 / 10.0]).collect();
        assert_eq!(interleave(&planes, 6), [0.1, 0.2, 0.0, 0.5, 0.3, 0.4]);
        // fewer channels than declared are silence, uneven lengths are cut to the shortest
        assert_eq!(interleave(&[vec![0.5, 0.5], vec![0.25]], 3), [0.5, 0.25, 0.0]);
        assert!(interleave(&[], 2).is_empty());
        assert_eq!(interleave(&[vec![f32::NAN, 7.0]], 1), [0.0, 1.0]);
    }
}
