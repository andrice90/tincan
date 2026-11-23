//! Soundboard and UI clips: decode any common audio file to 48 kHz mono PCM.

use crate::error::{AudioError, Result};
use crate::resample::resample_clip;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Sample rate of decoded clips.
pub const CLIP_RATE: u32 = 48_000;
/// Slack on top of `max_ms` for encoder padding (MP3/AAC priming and final-frame padding).
pub const CLIP_LENGTH_TOLERANCE_MS: u32 = 100;

/// A decoded clip: 48 kHz mono `f32` samples in -1..1. Wrap it in an `Arc` and reuse it for
/// every play; the engine never copies the samples.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClipPcm {
    samples: Vec<f32>,
}

impl ClipPcm {
    /// Wraps 48 kHz mono samples.
    pub fn from_samples(samples: Vec<f32>) -> Self {
        Self { samples }
    }

    /// The 48 kHz mono samples.
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Number of samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True if the clip has no samples.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Duration in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        self.samples.len() as u64 * 1000 / u64::from(CLIP_RATE)
    }
}

fn decode_err(what: &str, e: impl std::fmt::Display) -> AudioError {
    AudioError::ClipDecode(format!("{what}: {e}"))
}

/// Decodes an audio file (WAV, MP3, Ogg Vorbis, FLAC, M4A/AAC, MKV/WebM with a supported codec)
/// to 48 kHz mono. Multi-channel audio is averaged to mono; other rates are resampled once with
/// rubato. Fails with [`AudioError::ClipTooLong`] when the decoded audio is longer than
/// `max_ms` (+ [`CLIP_LENGTH_TOLERANCE_MS`] of padding slack); decoding stops as soon as the
/// limit is passed, so oversized files cost little.
pub fn decode_clip(bytes: &[u8], max_ms: u32) -> Result<ClipPcm> {
    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(&Hint::new(), mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| decode_err("unrecognized format", e))?;
    let (track_id, mut decoder) = {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| AudioError::ClipDecode("no audio track".into()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| AudioError::ClipDecode("no audio codec parameters".into()))?;
        let dec = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|e| decode_err("unsupported codec", e))?;
        (track.id, dec)
    };

    let mut mono: Vec<f32> = Vec::new();
    let mut tmp: Vec<f32> = Vec::new();
    let mut rate: Option<u32> = None;
    let limit_ms = u64::from(max_ms) + u64::from(CLIP_LENGTH_TOLERANCE_MS);
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            // A chained stream (new track list): keep what the first stream gave us.
            Err(SymError::ResetRequired) => break,
            Err(e) => return Err(decode_err("demux", e)),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(b) => b,
            // a corrupt packet: skip it, like every player does
            Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => continue,
            Err(e) => return Err(decode_err("decode", e)),
        };
        let spec_rate = buf.spec().rate();
        let channels = buf.spec().channels().count().max(1);
        match rate {
            None if spec_rate == 0 => return Err(AudioError::ClipDecode("sample rate 0".into())),
            None => rate = Some(spec_rate),
            Some(r) if r != spec_rate => {
                return Err(AudioError::ClipDecode("sample rate changes mid-stream".into()));
            }
            Some(_) => {}
        }
        buf.copy_to_vec_interleaved(&mut tmp);
        let inv = 1.0 / channels as f32;
        mono.extend(tmp.chunks_exact(channels).map(|f| f.iter().sum::<f32>() * inv));
        let r = u64::from(spec_rate);
        if mono.len() as u64 * 1000 > limit_ms * r {
            return Err(AudioError::ClipTooLong { duration_ms: mono.len() as u64 * 1000 / r, max_ms });
        }
    }
    let rate = rate.ok_or_else(|| AudioError::ClipDecode("no audio decoded".into()))?;
    if mono.is_empty() {
        return Err(AudioError::ClipDecode("no audio decoded".into()));
    }
    for s in &mut mono {
        *s = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
    }
    let samples = resample_clip(&mono, rate, CLIP_RATE)?;
    Ok(ClipPcm::from_samples(samples))
}
