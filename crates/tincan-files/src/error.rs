//! Error type of the crate.

use tincan_core::BlobHash;

/// Errors returned by [`crate::Files`].
#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    /// Filesystem error outside the store (paths, directories).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The blob store reported an error.
    #[error("blob store: {0}")]
    Store(String),
    /// The endpoint is closed or could not be configured.
    #[error("endpoint: {0}")]
    Endpoint(String),
    /// The blob is not (completely) held locally.
    #[error("blob {0:?} is not complete locally")]
    NotComplete(BlobHash),
    /// The blob is larger than the caller allowed.
    #[error("blob is {size} bytes, more than the allowed {max}")]
    TooLarge {
        /// Actual size.
        size: u64,
        /// The caller's limit.
        max: u64,
    },
    /// [`crate::Files::shutdown`] already ran.
    #[error("file store is shut down")]
    Closed,
}

/// Result alias for this crate.
pub type Result<T, E = FilesError> = std::result::Result<T, E>;

/// Wraps any blob-store error (they come in several iroh-blobs/irpc types).
pub(crate) fn store_err(e: impl std::fmt::Display) -> FilesError {
    FilesError::Store(e.to_string())
}
