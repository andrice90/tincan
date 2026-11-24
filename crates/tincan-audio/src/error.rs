//! Error type of the audio engine.

use thiserror::Error;

/// Errors returned by the audio engine, the Opus wrapper and clip decoding.
#[derive(Debug, Error)]
pub enum AudioError {
    /// `play_clip` was called while no call is running. Maps to the UI error string
    /// `not-in-voice`.
    #[error("not-in-voice")]
    NotInCall,
    /// `play_clip` was called while deafened (the soundboard is disabled while deafened).
    /// Maps to the UI error string `deafened`.
    #[error("deafened")]
    Deafened,
    /// The engine thread is gone (it panicked or the engine was shut down).
    #[error("audio engine stopped")]
    EngineStopped,
    /// No audio device of the requested kind exists.
    #[error("no {0} device")]
    NoDevice(&'static str),
    /// An audio device could not be enumerated or opened.
    #[error("audio device: {0}")]
    Device(String),
    /// The Opus codec reported an error (libopus error code and name).
    #[error("opus: {name} ({code})")]
    Codec {
        /// libopus error code (negative).
        code: i32,
        /// Symbolic name of the code.
        name: &'static str,
    },
    /// A soundboard clip is longer than allowed.
    #[error("clip too long: {duration_ms} ms (max {max_ms} ms)")]
    ClipTooLong {
        /// Decoded duration (lower bound when decoding stopped early).
        duration_ms: u64,
        /// The limit that was exceeded.
        max_ms: u32,
    },
    /// The clip could not be decoded (unknown format, unsupported codec, corrupt data, empty).
    #[error("clip decode: {0}")]
    ClipDecode(String),
    /// Resampling failed (invalid rates or buffer sizes).
    #[error("resample: {0}")]
    Resample(String),
    /// Spawning the engine or enumeration thread failed.
    #[error("thread: {0}")]
    Thread(String),
    /// Screen share audio cannot be captured here (process loopback unavailable or refused).
    #[error("share audio: {0}")]
    Unsupported(String),
}

/// Result alias using [`AudioError`].
pub type Result<T, E = AudioError> = std::result::Result<T, E>;
