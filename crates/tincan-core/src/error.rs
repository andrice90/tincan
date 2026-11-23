//! The one error type of tincan-core.

use thiserror::Error;

/// Errors returned by tincan-core functions.
#[derive(Debug, Error)]
pub enum CoreError {
    /// postcard encoding or decoding failed (malformed or truncated bytes).
    #[error("encoding: {0}")]
    Encode(#[from] postcard::Error),
    /// An ed25519 signature did not verify.
    #[error("bad signature")]
    BadSignature,
    /// 32 bytes that are not a valid ed25519 public key, or a key that does not match.
    #[error("bad key")]
    BadKey,
    /// A malformed event, or one that cannot be built as requested.
    #[error("invalid event: {0}")]
    InvalidEvent(String),
    /// An invite code that cannot be parsed or is not usable.
    #[error("invalid invite: {0}")]
    InvalidInvite(String),
    /// Encryption or decryption failed (wrong key or tampered data).
    #[error("crypto failure")]
    Crypto,
    /// A control frame larger than `wire::MAX_FRAME`.
    #[error("frame too large: {0} bytes")]
    FrameTooLarge(usize),
    /// A storage backend failure.
    #[error("storage: {0}")]
    Storage(String),
    /// Anything else (the string says what).
    #[error("{0}")]
    Other(String),
}

/// `Result` with [`CoreError`] as the default error.
pub type Result<T, E = CoreError> = std::result::Result<T, E>;
