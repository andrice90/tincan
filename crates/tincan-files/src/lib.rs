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
//! - Every get and observe request is intercepted too: the policy decides per (peer, hash)
//!   with [`AccessPolicy::allow_blob`], so an allowed peer still gets only the blobs it may
//!   see, and cannot probe which other blobs this node holds.
//! - Only single-blob requests are served. A get that asks for children (a hash sequence)
//!   would make the provider read a held blob as a list of hashes and serve those, so it is
//!   refused. Get-many requests are disabled.
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
//! Each kind has a hard size cap ([`BlobKind::max_size`]: 25 MiB for attachments, 5 MiB for
//! clips). The size a message declares is the sender's claim; the real size arrives with
//! the first verified chunk (or is already known for a partial blob), and a request whose
//! size, or whose verified bytes, pass the cap of every kind it was asked for stops there:
//! it turns [`TransferState::TooLarge`], its tags are dropped so garbage collection frees
//! the partial data, and it is not resumed or retried.
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

/// Largest attachment a download accepts: 25 MiB.
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
/// Largest soundboard clip a download accepts: 5 MiB.
pub const MAX_CLIP_BYTES: u64 = 5 * 1024 * 1024;
/// Largest avatar or server icon a download accepts: 64 KiB (`tincan_core::image`).
pub const MAX_IMAGE_BYTES: u64 = tincan_core::image::MAX_IMAGE_BYTES;

/// Decides which remote peers may fetch which blobs from this node.
///
/// Both methods run on the provider event loop, which also paces every uploaded chunk, so
/// they must be fast and must not block (no IO; a short lock is fine).
pub trait AccessPolicy: Send + Sync + 'static {
    /// True if `peer` may open a blobs connection at all. Called once per connection.
    fn allow(&self, peer: &UserId) -> bool;

    /// True if `peer` (already allowed to connect) may fetch or observe `hash`. Called once
    /// per get or observe request.
    fn allow_blob(&self, peer: &UserId, hash: &BlobHash) -> bool;
}

/// A closure decides per peer only: a peer it allows may fetch every blob this node holds.
/// Use a type that implements [`AccessPolicy::allow_blob`] for per-blob rules.
impl<F> AccessPolicy for F
where
    F: Fn(&UserId) -> bool + Send + Sync + 'static,
{
    fn allow(&self, peer: &UserId) -> bool {
        self(peer)
    }

    fn allow_blob(&self, peer: &UserId, _hash: &BlobHash) -> bool {
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
    /// An avatar or server icon (tag `img-<hex>`).
    Image,
}

impl BlobKind {
    /// Every kind, in a fixed order.
    pub const ALL: [BlobKind; 3] = [BlobKind::Attachment, BlobKind::Clip, BlobKind::Image];

    /// Tag name prefix for this kind, including the trailing dash.
    pub fn tag_prefix(self) -> &'static str {
        match self {
            BlobKind::Attachment => "att-",
            BlobKind::Clip => "clip-",
            BlobKind::Image => "img-",
        }
    }

    /// Full tag name for `hash` of this kind: prefix plus lowercase hex.
    pub fn tag_name(self, hash: &BlobHash) -> String {
        format!("{}{}", self.tag_prefix(), hash.to_hex())
    }

    /// The largest blob a download of this kind accepts ([`MAX_ATTACHMENT_BYTES`],
    /// [`MAX_CLIP_BYTES`] or [`MAX_IMAGE_BYTES`]).
    pub fn max_size(self) -> u64 {
        match self {
            BlobKind::Attachment => MAX_ATTACHMENT_BYTES,
            BlobKind::Clip => MAX_CLIP_BYTES,
            BlobKind::Image => MAX_IMAGE_BYTES,
        }
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
    /// The blob is larger than the cap of every kind it was requested as
    /// ([`BlobKind::max_size`]). The request stopped, its tags were dropped, and asking
    /// again for a kind with a smaller cap only repeats this state. `total` is the size
    /// that was refused.
    TooLarge,
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
        /// The provider that delivered the last verified bytes of the download. `None` for
        /// imports and for blobs that were already complete locally.
        from: Option<UserId>,
    },
}
