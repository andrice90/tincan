//! Tincan native audio engine.
//!
//! Capture (cpal/WASAPI) -> echo cancellation, noise suppression, AGC2 and high-pass (sonora)
//! -> Opus (20 ms, VOIP, 32 kbps VBR, FEC, DTX) -> 12-byte [`tincan_core::media::AudioHeader`]
//! datagrams; and received datagrams -> per-track adaptive jitter buffer -> Opus decode with FEC
//! and PLC -> mixer (per-track gain, soft limiter) -> speaker, with the exact speaker signal fed
//! back to the echo canceller. Soundboard clips are decoded with symphonia, resampled with rubato
//! and mixed into both the outgoing stream (after noise suppression) and local playout.
//!
//! Everything runs on one engine thread owned by [`AudioEngine`]; cpal callbacks only move
//! samples through lock-free rings. Internally the engine is 48 kHz mono with 10 ms frames.
//! See `docs/architecture.md` ("Audio", "Soundboard") for the protocol and `docs/notes-tincan-audio.md`
//! for implementation notes.
//!
//! ```no_run
//! use std::sync::Arc;
//! use tincan_audio::{AudioConfig, AudioEngine};
//! # async fn demo() -> tincan_audio::Result<()> {
//! let (engine, mut events) = AudioEngine::new(AudioConfig::default())?;
//! let (tx, mut datagrams) = tokio::sync::mpsc::channel(64);
//! engine.start_call(4, 1, tx)?; // our track id, host epoch
//! // send `datagrams` to the host; hand received ones to `engine.push_packet(bytes)`
//! # let _ = (&mut events, &mut datagrams);
//! engine.stop_call();
//! # Ok(()) }
//! ```

#![warn(missing_docs)]

mod apm;
pub mod codec;
mod clip;
mod device;
pub mod dsp;
mod engine;
mod error;
pub mod jitter;
#[cfg(windows)]
mod loopback;
mod processor;
mod resample;
mod ring;
mod share;

use std::sync::Arc;

pub use clip::{CLIP_LENGTH_TOLERANCE_MS, CLIP_RATE, ClipPcm, decode_clip};
pub use codec::{EncoderSettings, LibOpus, OpusCodec, OpusDecode, OpusEncode, StereoSettings};
pub use engine::AudioEngine;
pub use error::{AudioError, Result};
pub use jitter::JitterConfig;
#[cfg(windows)]
pub use loopback::{is_self_or_ancestor, window_process_id};
pub use share::{MAX_BACKLOG_MS, SHARE_FRAME, SHARE_MAX_PAYLOAD, ShareAudio, ShareAudioSource};

/// Samples in one 10 ms frame at 48 kHz: the unit [`AudioSource`] and [`AudioSink`] work in.
pub const FRAME_SAMPLES_10MS: usize = 480;
/// Internal sample rate.
pub const SAMPLE_RATE: u32 = 48_000;

/// Engine construction settings.
#[derive(Clone)]
pub struct AudioConfig {
    /// Input device id from [`AudioEngine::devices`] (`None` = system default).
    pub input_device: Option<String>,
    /// Output device id from [`AudioEngine::devices`] (`None` = system default).
    pub output_device: Option<String>,
    /// Echo cancellation on (default true).
    pub echo_cancellation: bool,
    /// Noise suppression on (default true).
    pub noise_suppression: bool,
    /// Microphone gain, 0..=4 (default 1).
    pub input_gain: f32,
    /// Speaker gain, 0..=4 (default 1).
    pub output_gain: f32,
    /// Jitter buffer bounds for new tracks (default 40/60/200 ms).
    pub jitter: JitterConfig,
    /// Opus implementation (default [`LibOpus`]).
    pub codec: Arc<dyn OpusCodec>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            echo_cancellation: true,
            noise_suppression: true,
            input_gain: 1.0,
            output_gain: 1.0,
            jitter: JitterConfig::default(),
            codec: Arc::new(LibOpus),
        }
    }
}

impl std::fmt::Debug for AudioConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioConfig")
            .field("input_device", &self.input_device)
            .field("output_device", &self.output_device)
            .field("echo_cancellation", &self.echo_cancellation)
            .field("noise_suppression", &self.noise_suppression)
            .field("input_gain", &self.input_gain)
            .field("output_gain", &self.output_gain)
            .field("jitter", &self.jitter)
            .finish_non_exhaustive()
    }
}

/// One audio device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Stable id to store and pass to [`AudioEngine::set_devices`].
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// True for the current system default device.
    pub is_default: bool,
}

/// Input and output devices, as returned by [`AudioEngine::devices`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AudioDevices {
    /// Capture devices.
    pub inputs: Vec<DeviceInfo>,
    /// Playback devices.
    pub outputs: Vec<DeviceInfo>,
}

/// Events from the engine. High-frequency ones (`Speaking`, `LocalSpeaking`, `MicLevel`) are
/// emitted on change or at about 20 Hz.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioEvent {
    /// A remote track started or stopped speaking (changes only): lit after 2 consecutive
    /// speaking packets, held 300 ms after the last one.
    Speaking {
        /// Track id from the audio header.
        track: u16,
        /// New state.
        speaking: bool,
    },
    /// The speaking flag we transmit changed (always false while muted).
    LocalSpeaking(bool),
    /// Microphone level 0..1 (-60..0 dBFS) while a mic test is on, about 20 per second.
    MicLevel(f32),
    /// Soundboard clip `id` (from [`AudioEngine::play_clip`]) is over: it was sent and played,
    /// stopped (`stop_clip`, `stop_clips`), cut by deafen, `stop_call` or a newer clip, or it
    /// could not start on the engine thread.
    ClipFinished(u64),
    /// A device problem, as a short user-facing sentence. The engine keeps retrying.
    DeviceError(String),
    /// The default device changed, a stream was re-routed, or a failed device recovered.
    /// Refresh the device list.
    DeviceChanged,
}

/// Source of microphone audio for [`AudioIo::Custom`].
pub trait AudioSource: Send {
    /// Fills `frame` (480 samples: 10 ms of 48 kHz mono, -1..1). Called every 10 ms while the
    /// engine wants input.
    fn read(&mut self, frame: &mut [f32]);
}

/// Sink for speaker audio for [`AudioIo::Custom`].
pub trait AudioSink: Send {
    /// Receives the next 10 ms (480 samples of 48 kHz mono) that would go to the speaker.
    fn write(&mut self, frame: &[f32]);
}

/// A source that always delivers silence (headless use without a microphone).
#[derive(Clone, Copy, Debug, Default)]
pub struct SilenceSource;

impl AudioSource for SilenceSource {
    fn read(&mut self, frame: &mut [f32]) {
        frame.fill(0.0);
    }
}

/// A sink that discards everything (headless use without a speaker).
#[derive(Clone, Copy, Debug, Default)]
pub struct NullSink;

impl AudioSink for NullSink {
    fn write(&mut self, _frame: &[f32]) {}
}

/// Where the engine gets and puts audio.
#[derive(Default)]
pub enum AudioIo {
    /// Real devices through cpal/WASAPI (default).
    #[default]
    Devices,
    /// A custom source and sink, driven by an internal 10 ms clock.
    Custom {
        /// Microphone replacement.
        source: Box<dyn AudioSource>,
        /// Speaker replacement.
        sink: Box<dyn AudioSink>,
    },
}

impl std::fmt::Debug for AudioIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioIo::Devices => f.write_str("Devices"),
            AudioIo::Custom { .. } => f.write_str("Custom"),
        }
    }
}
