//! Provider side: the iroh-blobs event loop (access control and upload throttling) and the
//! protocol handler the app registers on its Router.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_blobs::{
    BlobsProtocol,
    provider::events::{
        AbortReason, ConnectMode, EventMask, ObserveMode, ProviderMessage, RequestMode, ThrottleMode,
    },
};
use tokio::sync::mpsc;
use tracing::{debug, trace};

use crate::{AccessPolicy, Files, addr::user_id, throttle::UploadLimiter};

/// What the provider reports to [`run_events`].
///
/// - connections are intercepted (membership check),
/// - get requests are served without per-request events (the connection was already vetted),
/// - observe requests are allowed, so downloaders can ask which ranges we hold,
/// - pushes are refused,
/// - every sent chunk asks the upload limiter first.
pub(crate) const EVENT_MASK: EventMask = EventMask {
    connected: ConnectMode::Intercept,
    get: RequestMode::None,
    get_many: RequestMode::None,
    push: RequestMode::Disabled,
    observe: ObserveMode::None,
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

/// Answers provider events until the channel closes (the protocol was dropped) or the
/// task is aborted at shutdown. Decisions are never delayed inline: a throttled chunk is
/// released by a small timer task, so one slow upload never stalls the others.
pub(crate) async fn run_events(mut rx: mpsc::Receiver<ProviderMessage>, shared: Arc<ProviderShared>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            ProviderMessage::ClientConnected(msg) => {
                let verdict = match msg.inner.endpoint_id {
                    Some(id) if shared.policy.allow(&user_id(&id)) => {
                        trace!(peer = %id.fmt_short(), "blobs connection allowed");
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
            // Not produced with EVENT_MASK; answered anyway so nothing can hang.
            ProviderMessage::GetRequestReceived(msg) => {
                msg.tx.send(Ok(())).await.ok();
            }
            ProviderMessage::GetManyRequestReceived(msg) => {
                msg.tx.send(Ok(())).await.ok();
            }
            ProviderMessage::ObserveRequestReceived(msg) => {
                msg.tx.send(Ok(())).await.ok();
            }
            ProviderMessage::PushRequestReceived(msg) => {
                msg.tx.send(Err(AbortReason::Permission)).await.ok();
            }
            ProviderMessage::ClientConnectedNotify(_)
            | ProviderMessage::ConnectionClosed(_)
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
