//! Tincan file swarm: attachments and soundboard clips over iroh-blobs.
//!
//! Every Tincan peer keeps an iroh-blobs [`FsStore`](iroh_blobs::store::fs::FsStore) in
//! its profile directory, serves what it holds to other members over
//! [`iroh_blobs::ALPN`], and downloads from whoever holds a blob (BLAKE3 verified,
//! resumable, from several providers in turn). Because every downloader also seeds,
//! a file stays available after its uploader goes offline.
//!
//! # Wiring
//!
//! ```text
//! TOKIO_WORKER_THREADS=2 set before any runtime exists   (see Files::load)
//! let endpoint = tincan_net::build_endpoint(..).await?;
//! let files = Files::load(&profile_dir.join("blobs"), endpoint.clone(), policy).await?;
//! router: .accept(iroh_blobs::ALPN, files.protocol())    (the app's shared Router)
//! ...
//! router.shutdown().await;   // also shuts `files` down cleanly (idempotent)
//! ```
//!
//! [`Files`] does not spawn a Router. The app registers [`Files::protocol`] on its shared
//! Router under [`iroh_blobs::ALPN`]. Shutting the Router down shuts the store down through
//! that handler; [`Files::shutdown`] does the same and may be called in either order.
//!
//! # Provider side
//!
//! - Connections are intercepted: a peer whose [`UserId`] the [`AccessPolicy`] denies is
//!   rejected with `AbortReason::Permission` before it can send a request.
//! - Observe requests are enabled, so a downloader can ask a peer which ranges it holds.
//! - Uploads pass through a global token bucket when [`Files::set_upload_limit`] sets a cap.
//! - Push requests (peers writing into our store) are disabled.
//!
//! # Downloader side
//!
//! [`Files::download`] queues a request (idempotent). Providers are tried one at a time in
//! the order given (author first); each one is asked only for the ranges still missing, so
//! a partial holder contributes what it has and the next provider completes the rest. A
//! provider that does not answer costs the 4 s connect timeout. When every known provider
//! has failed, the request turns [`TransferState::Unavailable`] and stays parked until
//! [`Files::add_providers`] names a holder again. Progress is reported on
//! [`Files::subscribe`].
//!
//! Hashes being downloaded or held are tagged `att-<hex>` or `clip-<hex>` in the store, so
//! garbage collection (hourly) keeps them, partial downloads included, until
//! [`Files::remove`] drops the tag.

#![warn(missing_docs)]

mod addr;
mod download;
mod error;
mod files;
mod provider;
mod throttle;

pub use error::{FilesError, Result};
pub use files::Files;
pub use provider::FilesProtocol;

/// ALPN to register [`Files::protocol`] under (`/iroh-bytes/4`, same as [`iroh_blobs::ALPN`]).
pub const ALPN: &[u8] = iroh_blobs::ALPN;

use tincan_core::{BlobHash, UserId};

/// Decides which remote peers may fetch blobs from this node.
///
/// The app implements it as "member of a server we share, or a DM contact". It is called
/// once per incoming blobs connection from the provider event loop, so it must be fast
/// and must not block (no IO; a short lock is fine).
pub trait AccessPolicy: Send + Sync + 'static {
    /// True if `peer` may connect and fetch blobs.
    fn allow(&self, peer: &UserId) -> bool;
}

impl<F> AccessPolicy for F
where
    F: Fn(&UserId) -> bool + Send + Sync + 'static,
{
    fn allow(&self, peer: &UserId) -> bool {
        self(peer)
    }
}

/// What a blob is used for. Each kind has its own named tag in the store, so the same
/// bytes can be both an attachment and a clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlobKind {
    /// A message attachment (tag `att-<hex>`).
    Attachment,
    /// A soundboard clip (tag `clip-<hex>`).
    Clip,
}

impl BlobKind {
    /// Both kinds, in a fixed order.
    pub const ALL: [BlobKind; 2] = [BlobKind::Attachment, BlobKind::Clip];

    /// Tag name prefix for this kind, including the trailing dash.
    pub fn tag_prefix(self) -> &'static str {
        match self {
            BlobKind::Attachment => "att-",
            BlobKind::Clip => "clip-",
        }
    }

    /// Full tag name for `hash` of this kind: prefix plus lowercase hex.
    pub fn tag_name(self, hash: &BlobHash) -> String {
        format!("{}{}", self.tag_prefix(), hash.to_hex())
    }
}

/// Result of importing local data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Imported {
    /// BLAKE3 hash of the content.
    pub hash: BlobHash,
    /// Size in bytes.
    pub size: u64,
}

/// What the local store holds of a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalStatus {
    /// All bytes are present and verified.
    pub complete: bool,
    /// Verified bytes held locally.
    pub have: u64,
    /// Best known size: verified once complete, otherwise as reported by the provider.
    /// `None` when nothing is known about the blob.
    pub total: Option<u64>,
}

/// State of a download request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransferState {
    /// Waiting for a download slot (a few downloads run at once).
    Queued,
    /// Fetching from a provider.
    Downloading,
    /// Every byte is present and verified.
    Complete,
    /// Every known provider failed. Parked until [`Files::add_providers`] brings a holder.
    Unavailable,
    /// A local error (store or disk) stopped the download. Call [`Files::download`] again
    /// to retry.
    Failed,
}

/// Snapshot of one download request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferInfo {
    /// The blob being fetched.
    pub hash: BlobHash,
    /// What it is for.
    pub kind: BlobKind,
    /// Current state.
    pub state: TransferState,
    /// Verified bytes held locally.
    pub have: u64,
    /// Size of the blob once a provider reported it.
    pub total: Option<u64>,
    /// Known providers not yet ruled out in the current round (0 when unavailable).
    pub peers: u32,
}

/// Events on [`Files::subscribe`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileEvent {
    /// A download request changed state or made progress. Progress is coalesced to at
    /// most about four events per second per download; state changes are always sent.
    Transfer(TransferInfo),
    /// A blob became complete locally, by download or import. The app announces it in
    /// `Have`. Also sent when an already complete blob gains a second kind.
    Completed {
        /// The blob.
        hash: BlobHash,
        /// The kind it was completed (tagged) as.
        kind: BlobKind,
    },
}
