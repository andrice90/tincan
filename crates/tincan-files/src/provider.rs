//! Provider side: the iroh-blobs event loop (access control and upload throttling) and the
//! protocol handler the app registers on its Router.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_blobs::{
    BlobsProtocol,
    protocol::ChunkRangesSeq,
    provider::events::{
        AbortReason, ConnectMode, EventMask, EventResult, ObserveMode, ProviderMessage, RequestMode, ThrottleMode,
    },
};
use tincan_core::{BlobHash, UserId};
use tokio::sync::mpsc;
use tracing::{debug, trace};

use crate::{
    AccessPolicy, Files,
    addr::{blob_hash, user_id},
    throttle::UploadLimiter,
};

/// What the provider reports to [`run_events`].
///
/// - connections are intercepted (the peer check),
/// - get and observe requests are intercepted (the per-blob check; observe stays available
///   to allowed peers, so a downloader can ask which ranges a holder has),
/// - get-many and push requests are refused,
/// - every sent chunk asks the upload limiter first.
pub(crate) const EVENT_MASK: EventMask = EventMask {
    connected: ConnectMode::Intercept,
    get: RequestMode::Intercept,
    get_many: RequestMode::Disabled,
    push: RequestMode::Disabled,
    observe: ObserveMode::Intercept,
    throttle: ThrottleMode::Intercept,
};

/// Capacity of the provider event channel. Every chunk sent produces one message, so it
/// must not be tiny; the loop answers most of them immediately.
pub(crate) const EVENT_CAPACITY: usize = 64;

/// State shared between [`crate::Files`] and the provider event loop.
pub(crate) struct ProviderShared {
    pub(crate) policy: Arc<dyn AccessPolicy>,
    pub(crate) limiter: UploadLimiter,
    /// Payload bytes sent to peers since load.
    pub(crate) uploaded: AtomicU64,
}

/// The verdict on one get request. `peer` is the connection's vetted user (`None` when the
/// connection is unknown). Only a request for the root blob alone is served: child ranges
/// make iroh-blobs read the root as a hash sequence and send the blobs it lists, which would
/// bypass the per-blob check.
pub(crate) fn get_verdict(
    policy: &dyn AccessPolicy,
    peer: Option<&UserId>,
    hash: &BlobHash,
    ranges: &ChunkRangesSeq,
) -> EventResult {
    if !ranges.is_blob() {
        return Err(AbortReason::Permission);
    }
    blob_verdict(policy, peer, hash)
}

/// The verdict on one observe request (and the blob part of a get).
pub(crate) fn blob_verdict(policy: &dyn AccessPolicy, peer: Option<&UserId>, hash: &BlobHash) -> EventResult {
    match peer {
        Some(p) if policy.allow_blob(p, hash) => Ok(()),
        _ => Err(AbortReason::Permission),
    }
}

/// Answers provider events until the channel closes (the protocol was dropped) or the
/// task is aborted at shutdown. Decisions are never delayed inline: a throttled chunk is
/// released by a small timer task, so one slow upload never stalls the others.
pub(crate) async fn run_events(mut rx: mpsc::Receiver<ProviderMessage>, shared: Arc<ProviderShared>) {
    // Vetted connections. Request events carry only the connection id, so the peer is
    // remembered from the connect event and forgotten when the connection closes.
    let mut peers: HashMap<u64, UserId> = HashMap::new();
    let policy = shared.policy.as_ref();
    while let Some(msg) = rx.recv().await {
        match msg {
            ProviderMessage::ClientConnected(msg) => {
                let verdict = match msg.inner.endpoint_id {
                    Some(id) if policy.allow(&user_id(&id)) => {
                        trace!(peer = %id.fmt_short(), "blobs connection allowed");
                        peers.insert(msg.inner.connection_id, user_id(&id));
                        Ok(())
                    }
                    Some(id) => {
                        debug!(peer = %id.fmt_short(), "blobs connection denied by policy");
                        Err(AbortReason::Permission)
                    }
                    None => {
                        debug!("blobs connection without endpoint id denied");
                        Err(AbortReason::Permission)
                    }
                };
                msg.tx.send(verdict).await.ok();
            }
            ProviderMessage::ConnectionClosed(msg) => {
                peers.remove(&msg.inner.connection_id);
            }
            ProviderMessage::GetRequestReceived(msg) => {
                let req = &msg.inner.request;
                let hash = blob_hash(&req.hash);
                let peer = peers.get(&msg.inner.connection_id);
                let verdict = get_verdict(policy, peer, &hash, &req.ranges);
                if verdict.is_err() {
                    debug!(hash = %hash.short(), peer = ?peer.map(UserId::short), "get request denied");
                }
                msg.tx.send(verdict).await.ok();
            }
            ProviderMessage::ObserveRequestReceived(msg) => {
                let hash = blob_hash(&msg.inner.request.hash);
                let peer = peers.get(&msg.inner.connection_id);
                let verdict = blob_verdict(policy, peer, &hash);
                if verdict.is_err() {
                    debug!(hash = %hash.short(), peer = ?peer.map(UserId::short), "observe request denied");
                }
                msg.tx.send(verdict).await.ok();
            }
            ProviderMessage::Throttle(msg) => {
                let size = msg.inner.size;
                shared.uploaded.fetch_add(size, Ordering::Relaxed);
                let wait = shared.limiter.reserve(size);
                if wait.is_zero() {
                    msg.tx.send(Ok(())).await.ok();
                } else {
                    let tx = msg.tx;
                    tokio::spawn(async move {
                        tokio::time::sleep(wait).await;
                        tx.send(Ok(())).await.ok();
                    });
                }
            }
            // Not produced with EVENT_MASK (disabled); refused anyway so nothing can hang.
            ProviderMessage::GetManyRequestReceived(msg) => {
                msg.tx.send(Err(AbortReason::Permission)).await.ok();
            }
            ProviderMessage::PushRequestReceived(msg) => {
                msg.tx.send(Err(AbortReason::Permission)).await.ok();
            }
            ProviderMessage::ClientConnectedNotify(_)
            | ProviderMessage::GetRequestReceivedNotify(_)
            | ProviderMessage::GetManyRequestReceivedNotify(_)
            | ProviderMessage::PushRequestReceivedNotify(_)
            | ProviderMessage::ObserveRequestReceivedNotify(_) => {}
        }
    }
    debug!("blobs provider event loop ended");
}

/// The blobs protocol handler to register on the app's Router under [`iroh_blobs::ALPN`].
///
/// Serves the local store to peers the [`AccessPolicy`] allows. When the Router shuts
/// down it calls [`Files::shutdown`], which stops downloads and closes the store cleanly
/// (partial downloads survive). Cheap to clone.
#[derive(Debug, Clone)]
pub struct FilesProtocol {
    pub(crate) blobs: BlobsProtocol,
    pub(crate) files: Files,
}

impl ProtocolHandler for FilesProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if self.files.is_closed() {
            connection.close(0u32.into(), b"shutting down");
            return Ok(());
        }
        // Delegates to iroh-blobs; the event loop vets the peer before any request is read.
        ProtocolHandler::accept(&self.blobs, connection).await
    }

    async fn shutdown(&self) {
        // Not BlobsProtocol::shutdown: Files owns the store's lifecycle, so the store is
        // shut down exactly once, after downloads stopped.
        self.files.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use iroh_blobs::protocol::{ChunkRanges, ChunkRangesExt, GetRequest};

    use super::*;

    struct OnlyFor(UserId, BlobHash);

    impl AccessPolicy for OnlyFor {
        fn allow(&self, peer: &UserId) -> bool {
            *peer == self.0
        }
        fn allow_blob(&self, peer: &UserId, hash: &BlobHash) -> bool {
            *peer == self.0 && *hash == self.1
        }
    }

    #[test]
    fn only_single_blob_requests_for_allowed_hashes_pass() {
        let peer = UserId::from_bytes([1; 32]);
        let other = UserId::from_bytes([2; 32]);
        let hash = BlobHash::from_bytes([3; 32]);
        let secret = BlobHash::from_bytes([4; 32]);
        let policy = OnlyFor(peer, hash);
        let h = crate::addr::blobs_hash(&hash);

        let blob = GetRequest::blob(h).ranges;
        assert!(get_verdict(&policy, Some(&peer), &hash, &blob).is_ok());
        assert!(get_verdict(&policy, Some(&peer), &secret, &blob).is_err(), "hash not allowed");
        assert!(get_verdict(&policy, Some(&other), &hash, &blob).is_err(), "peer not allowed");
        assert!(get_verdict(&policy, None, &hash, &blob).is_err(), "unknown connection");

        // A partial re-request (what the downloader sends for missing ranges) is one blob.
        let partial = GetRequest::builder().root(ChunkRanges::chunks(4..)).build(h).ranges;
        assert!(get_verdict(&policy, Some(&peer), &hash, &partial).is_ok());

        // Children of the root (a hash sequence) are never served.
        assert!(get_verdict(&policy, Some(&peer), &hash, &GetRequest::all(h).ranges).is_err());
        let child = GetRequest::builder().child(0, ChunkRanges::all()).build(h).ranges;
        assert!(get_verdict(&policy, Some(&peer), &hash, &child).is_err());
        let both = GetRequest::builder().root(ChunkRanges::all()).child(0, ChunkRanges::all()).build(h).ranges;
        assert!(get_verdict(&policy, Some(&peer), &hash, &both).is_err());

        assert!(blob_verdict(&policy, Some(&peer), &hash).is_ok());
        assert!(blob_verdict(&policy, Some(&peer), &secret).is_err());
    }
}
