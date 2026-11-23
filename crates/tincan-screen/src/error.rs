//! Errors returned by tincan-screen.

/// Result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a screen share could not be listed, started or continued.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// This machine has no hardware H.264 encoder (no GPU, a VM, or a driver
    /// without a Media Foundation encoder). The app reports it to the UI as the
    /// machine-readable string `no-hardware-encoder` (see [`Error::code`]).
    #[error("no hardware H.264 encoder on this machine")]
    NoHardwareEncoder,
    /// The source id is not one [`sources`](crate::sources) produces.
    #[error("not a screen source id: {0:?}")]
    InvalidSource(String),
    /// The display or window does not exist (any more), or is minimized.
    #[error("screen source unavailable: {0}")]
    SourceUnavailable(String),
    /// Windows refused capture of this source.
    #[error("screen capture permission denied: {0}")]
    PermissionDenied(String),
    /// Windows.Graphics.Capture is missing (needs Windows 10 2004 or newer).
    #[error("screen capture is not supported here: {0}")]
    Unsupported(String),
    /// Capture failed for another reason (driver, device lost).
    #[error("screen capture failed: {0}")]
    Capture(String),
    /// The hardware encoder exists but failed to open or encode.
    #[error("video encoder failed: {0}")]
    Encoder(String),
    /// The capture/encode thread could not be started or died.
    #[error("screen share thread failed: {0}")]
    Thread(String),
}

impl Error {
    /// The machine-readable error string from `docs/api.md`, if this error has
    /// one: `Some("no-hardware-encoder")` for [`Error::NoHardwareEncoder`].
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Error::NoHardwareEncoder => Some("no-hardware-encoder"),
            _ => None,
        }
    }

    /// Map a moq-video error from capture (open, read, enumerate).
    pub(crate) fn from_capture(err: moq_video::Error) -> Error {
        match err {
            moq_video::Error::SourceUnavailable(m) => Error::SourceUnavailable(m),
            moq_video::Error::PermissionDenied(m) => Error::PermissionDenied(m),
            moq_video::Error::Unsupported(m) => Error::Unsupported(m),
            other => Error::Capture(other.to_string()),
        }
    }

    /// Map a moq-video error from the encoder (open, encode, scale).
    pub(crate) fn from_encoder(err: moq_video::Error) -> Error {
        match err {
            moq_video::Error::NoEncoder(detail) => {
                tracing::debug!(%detail, "no hardware encoder opened");
                Error::NoHardwareEncoder
            }
            other => Error::Encoder(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_missing_encoder_has_an_api_code() {
        assert_eq!(Error::NoHardwareEncoder.code(), Some("no-hardware-encoder"));
        assert_eq!(Error::InvalidSource("x".into()).code(), None);
        assert_eq!(Error::Encoder("x".into()).code(), None);
    }

    #[test]
    fn moq_errors_map_to_distinct_variants() {
        assert_eq!(
            Error::from_encoder(moq_video::Error::NoEncoder("mediafoundation: none".into())),
            Error::NoHardwareEncoder
        );
        assert_eq!(
            Error::from_capture(moq_video::Error::SourceUnavailable("window:1 closed".into())),
            Error::SourceUnavailable("window:1 closed".into())
        );
        assert_eq!(
            Error::from_capture(moq_video::Error::PermissionDenied("no".into())),
            Error::PermissionDenied("no".into())
        );
        assert!(matches!(
            Error::from_encoder(moq_video::Error::CutUnsupported("x")),
            Error::Encoder(_)
        ));
    }
}
