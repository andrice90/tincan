//! The error type of tincan-net. Display strings are short and user-presentable.

use thiserror::Error;
use tincan_core::CoreError;

/// Errors returned by the public API of tincan-net.
///
/// `Display` gives a short sentence that can be shown to the user as is.
#[derive(Debug, Error)]
pub enum NetError {
    /// A tincan-core failure (encoding, signing, storage, validation).
    #[error("{0}")]
    Core(#[from] CoreError),
    /// The peer or host turned the request away; the text is tincan-net's own sentence and
    /// says why.
    #[error("{0}")]
    Rejected(String),
    /// The host turned the request away in its own words (why it refused a publish or a
    /// join). Cleaned of control characters and at most 200 characters, but still text from
    /// a peer: show it as the host's words, never as Tincan's own sentence.
    #[error("{0}")]
    RejectedByPeer(String),
    /// Nobody could be reached; the text says what to do.
    #[error("{0}")]
    Unreachable(String),
    /// The server is not connected to a host right now.
    #[error("Not connected to this server right now. Try again in a moment.")]
    NotConnected,
    /// No server or conversation with that id is known locally.
    #[error("Unknown server or conversation.")]
    UnknownConv,
    /// The network layer is shutting down or already shut down.
    #[error("Tincan is shutting down.")]
    Closed,
    /// The operation did not finish in time.
    #[error("That took too long. Try again.")]
    Timeout,
    /// The input was not acceptable; the text says why.
    #[error("{0}")]
    Invalid(String),
    /// Binding or configuring the iroh endpoint failed.
    #[error("Network setup failed: {0}")]
    Endpoint(String),
    /// The operation needs a voice session (room or call). Displays as the machine-readable
    /// string `not-in-voice` from docs/api.md.
    #[error("not-in-voice")]
    NotInVoice,
}

/// `Result` with [`NetError`] as the default error.
pub type Result<T, E = NetError> = std::result::Result<T, E>;

impl NetError {
    /// Turn a core invite/event error into a user-facing message.
    pub(crate) fn from_core_user(e: CoreError) -> Self {
        match e {
            CoreError::InvalidInvite(s) | CoreError::InvalidEvent(s) => NetError::Invalid(capitalize(&s)),
            other => NetError::Core(other),
        }
    }
}

/// First letter uppercase, and a trailing period.
pub(crate) fn capitalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 1);
    let mut chars = s.chars();
    if let Some(c) = chars.next() {
        out.extend(c.to_uppercase());
        out.push_str(chars.as_str());
    }
    if !out.ends_with('.') && !out.ends_with('!') && !out.ends_with('?') {
        out.push('.');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capitalize_adds_period() {
        assert_eq!(capitalize("bad thing"), "Bad thing.");
        assert_eq!(capitalize("Done."), "Done.");
        assert_eq!(capitalize(""), ".");
    }
}
