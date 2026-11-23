//! Control-stream plumbing: frame reader, per-connection reader/writer tasks, dialing and a
//! task tracker.
//!
//! Every connection carries one bidirectional control stream (the dialer opens it and writes
//! first). Reads happen in a dedicated task per link, never inside a `select!` arm, so a
//! frame is never cut in half by cancellation. Writes go through two queues drained by a
//! writer task: a bounded "live" queue filled with `try_send` (overflow means the peer is too
//! slow and the caller drops it) and a small "bulk" queue for history pages, filled with
//! backpressure. Live frames always go first.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use iroh::Endpoint;
use iroh::endpoint::{Connection, ConnectionError, RecvStream, SendStream, VarInt};
use parking_lot::Mutex;
use tincan_core::wire::{self, Ctl, FrameDecoder};
use tincan_core::{CoreError, UserId};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::addr::endpoint_addr;
use tincan_core::wire::AddrHint;

/// Bytes requested per stream read.
const READ_CHUNK: usize = 64 * 1024;
/// Live queue length (frames). Overflow = the peer is too slow.
pub(crate) const LIVE_QUEUE: usize = 1024;
/// Bulk queue length (history pages, backpressured).
const BULK_QUEUE: usize = 2;
/// Priority of the control stream (video GOP streams use 0..50).
const CONTROL_PRIORITY: i32 = 100;

/// Encode a control message as one frame. Errors (only oversized frames) are logged.
pub(crate) fn encode(ctl: &Ctl) -> Option<Bytes> {
    match wire::encode_frame(ctl) {
        Ok(v) => Some(Bytes::from(v)),
        Err(e) => {
            tracing::warn!(error = %e, "control frame not encodable; dropped");
            None
        }
    }
}

/// Why reading frames stopped.
#[derive(Debug)]
pub(crate) enum ReadEnd {
    /// The peer finished the stream.
    Eof,
    /// The stream or connection failed.
    Failed(String),
}

impl std::fmt::Display for ReadEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadEnd::Eof => f.write_str("end of stream"),
            ReadEnd::Failed(why) => f.write_str(why),
        }
    }
}

/// Reads length-prefixed `Ctl` frames from a stream.
#[derive(Debug)]
pub(crate) struct FrameReader {
    recv: RecvStream,
    dec: FrameDecoder,
}

impl FrameReader {
    /// Wrap a receive stream.
    pub fn new(recv: RecvStream) -> Self { Self { recv, dec: FrameDecoder::new() } }

    /// Next message. Undecodable frames (for example from a newer peer) are skipped.
    pub async fn next(&mut self) -> Result<Ctl, ReadEnd> {
        loop {
            match self.dec.next() {
                Ok(Some(c)) => return Ok(c),
                Ok(None) => {}
                Err(CoreError::FrameTooLarge(n)) => return Err(ReadEnd::Failed(format!("frame of {n} bytes"))),
                Err(e) => {
                    tracing::debug!(error = %e, "skipping undecodable control frame");
                    continue;
                }
            }
            match self.recv.read_chunk(READ_CHUNK).await {
                Ok(Some(b)) => self.dec.push(&b),
                Ok(None) => return Err(ReadEnd::Eof),
                Err(e) => return Err(ReadEnd::Failed(e.to_string())),
            }
        }
    }
}

/// Write one frame directly (before a link's writer task exists).
pub(crate) async fn write_ctl(send: &mut SendStream, ctl: &Ctl) -> Result<(), String> {
    let bytes = encode(ctl).ok_or_else(|| "frame too large".to_string())?;
    send.write_all(&bytes).await.map_err(|e| e.to_string())
}

/// The application close code of a closed connection, if the peer closed it on purpose.
pub(crate) fn close_code(err: &ConnectionError) -> Option<u64> {
    match err {
        ConnectionError::ApplicationClosed(ac) => Some(u64::from(ac.error_code)),
        _ => None,
    }
}

/// Close a connection with one of the `tincan_core::CLOSE_*` codes.
pub(crate) fn close_conn(conn: &Connection, code: u32, reason: &str) { conn.close(VarInt::from_u32(code), reason.as_bytes()); }

/// Tracks spawned tasks so they can all be aborted (shutdown and crash tests).
#[derive(Debug, Default)]
pub(crate) struct Tasks {
    next: AtomicU64,
    map: Mutex<HashMap<u64, AbortHandle>>,
}

impl Tasks {
    /// Spawn a tracked task; it unregisters itself when it ends.
    pub fn spawn<F>(self: &Arc<Self>, fut: F) -> AbortHandle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let weak = Arc::downgrade(self);
        let mut map = self.map.lock();
        let handle = tokio::spawn(async move {
            fut.await;
            if let Some(t) = weak.upgrade() {
                t.map.lock().remove(&id);
            }
        });
        let abort = handle.abort_handle();
        map.insert(id, abort.clone());
        abort
    }

    /// Abort every tracked task.
    pub fn abort_all(&self) {
        let handles: Vec<AbortHandle> = self.map.lock().drain().map(|(_, h)| h).collect();
        for h in handles {
            h.abort();
        }
    }
}

/// Something that happened on a link, delivered to the owning actor.
#[derive(Debug)]
pub(crate) enum LinkEvent {
    /// A control frame arrived.
    Frame {
        /// Link id.
        link: u64,
        /// The message (boxed: `Ctl` is large and most frames are small).
        ctl: Box<Ctl>,
    },
    /// The link ended. `code` is the peer's application close code, if it closed on purpose.
    Closed {
        /// Link id.
        link: u64,
        /// Application close code.
        code: Option<u64>,
    },
    /// A history sync sender on this link finished sending.
    SyncSent {
        /// Link id.
        link: u64,
    },
}

/// One live control-stream connection with its reader and writer tasks.
#[derive(Debug)]
pub(crate) struct Link {
    /// Actor-local id.
    pub id: u64,
    /// Authenticated peer.
    pub peer: UserId,
    /// The connection (part B rides media on it).
    pub conn: Connection,
    live: mpsc::Sender<Bytes>,
    bulk: mpsc::Sender<Bytes>,
    reader: AbortHandle,
    writer: AbortHandle,
}

impl Link {
    /// Start reader and writer tasks. Frames and the final close go to `events`.
    pub fn spawn(
        tasks: &Arc<Tasks>,
        id: u64,
        conn: Connection,
        mut send: SendStream,
        mut reader: FrameReader,
        events: mpsc::Sender<LinkEvent>,
    ) -> Link {
        let peer = crate::addr::remote_user(&conn);
        let _ = send.set_priority(CONTROL_PRIORITY);
        let (live, mut live_rx) = mpsc::channel::<Bytes>(LIVE_QUEUE);
        let (bulk, mut bulk_rx) = mpsc::channel::<Bytes>(BULK_QUEUE);
        let writer = tasks.spawn(async move {
            loop {
                let next = tokio::select! {
                    biased;
                    b = live_rx.recv() => b,
                    b = bulk_rx.recv() => b,
                };
                let Some(bytes) = next else { break };
                if let Err(e) = send.write_all(&bytes).await {
                    tracing::debug!(error = %e, "control stream write failed");
                    break;
                }
            }
            let _ = send.finish();
        });
        let rconn = conn.clone();
        let reader = tasks.spawn(async move {
            loop {
                match reader.next().await {
                    Ok(ctl) => {
                        if events.send(LinkEvent::Frame { link: id, ctl: Box::new(ctl) }).await.is_err() {
                            return;
                        }
                    }
                    Err(end) => {
                        tracing::debug!(link = id, reason = %end, "control stream ended");
                        break;
                    }
                }
            }
            // The stream ended; the connection is useless without it.
            let code = match rconn.close_reason() {
                Some(e) => close_code(&e),
                None => {
                    close_conn(&rconn, tincan_core::CLOSE_NORMAL, "control stream ended");
                    None
                }
            };
            let _ = events.send(LinkEvent::Closed { link: id, code }).await;
        });
        Link { id, peer, conn, live, bulk, reader, writer }
    }

    /// Queue a frame on the live queue. False if the queue is full or the link is gone.
    pub fn send(&self, ctl: &Ctl) -> bool {
        match encode(ctl) {
            Some(b) => self.send_bytes(b),
            None => true,
        }
    }

    /// Queue pre-encoded bytes on the live queue.
    pub fn send_bytes(&self, b: Bytes) -> bool { self.live.try_send(b).is_ok() }

    /// A sender for history pages (awaits queue space).
    pub fn bulk(&self) -> mpsc::Sender<Bytes> { self.bulk.clone() }

    /// A sender on the live queue (part B hooks, flush-then-close helpers).
    pub fn live(&self) -> mpsc::Sender<Bytes> { self.live.clone() }

    /// Close the connection now.
    pub fn close(&self, code: u32, reason: &str) { close_conn(&self.conn, code, reason); }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

/// A connection that finished the first request/response exchange on its control stream.
#[derive(Debug)]
pub(crate) struct Dialed {
    /// The connection.
    pub conn: Connection,
    /// Control stream, send half.
    pub send: SendStream,
    /// Control stream, receive half (may hold already-buffered frames).
    pub reader: FrameReader,
    /// The peer's first reply.
    pub reply: Ctl,
}

/// Why a dial failed.
#[derive(Debug, Clone)]
pub(crate) enum DialError {
    /// Not reachable within the timeout, or the connection failed.
    Unreachable(String),
    /// The peer answered but closed before replying.
    NoReply(String),
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DialError::Unreachable(s) => write!(f, "unreachable: {s}"),
            DialError::NoReply(s) => write!(f, "no reply: {s}"),
        }
    }
}

/// Connect to `user` on `alpn` (whole attempt bounded by `timeout`).
pub(crate) async fn connect(
    ep: &Endpoint,
    user: &UserId,
    hint: Option<&AddrHint>,
    alpn: &[u8],
    timeout: Duration,
) -> Result<Connection, DialError> {
    let addr = endpoint_addr(user, hint).ok_or_else(|| DialError::Unreachable("invalid user id".into()))?;
    match tokio::time::timeout(timeout, ep.connect(addr, alpn)).await {
        Ok(Ok(c)) => Ok(c),
        Ok(Err(e)) => Err(DialError::Unreachable(e.to_string())),
        Err(_) => Err(DialError::Unreachable("timed out".into())),
    }
}

/// Open the control stream on `conn`, send `first`, and wait for the first reply.
pub(crate) async fn exchange(conn: Connection, first: &Ctl, timeout: Duration) -> Result<Dialed, DialError> {
    let fut = async {
        let (mut send, recv) = conn.open_bi().await.map_err(|e| DialError::Unreachable(e.to_string()))?;
        write_ctl(&mut send, first).await.map_err(DialError::Unreachable)?;
        let mut reader = FrameReader::new(recv);
        let reply = reader.next().await.map_err(|e| {
            let why = match conn.close_reason() {
                Some(r) => r.to_string(),
                None => format!("{e:?}"),
            };
            DialError::NoReply(why)
        })?;
        Ok::<_, DialError>((send, reader, reply))
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok((send, reader, reply))) => Ok(Dialed { conn, send, reader, reply }),
        Ok(Err(e)) => {
            close_conn(&conn, tincan_core::CLOSE_NORMAL, "no reply");
            Err(e)
        }
        Err(_) => {
            close_conn(&conn, tincan_core::CLOSE_NORMAL, "timeout");
            Err(DialError::NoReply("timed out".into()))
        }
    }
}

/// Continue the exchange on an already open control stream: send `next`, read one reply.
pub(crate) async fn continue_exchange(
    conn: Connection,
    mut send: SendStream,
    mut reader: FrameReader,
    next: &Ctl,
    timeout: Duration,
) -> Result<Dialed, DialError> {
    let fut = async {
        write_ctl(&mut send, next).await.map_err(DialError::Unreachable)?;
        reader.next().await.map_err(|e| DialError::NoReply(format!("{e:?}")))
    };
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(reply)) => Ok(Dialed { conn, send, reader, reply }),
        Ok(Err(e)) => {
            close_conn(&conn, tincan_core::CLOSE_NORMAL, "no reply");
            Err(e)
        }
        Err(_) => {
            close_conn(&conn, tincan_core::CLOSE_NORMAL, "timeout");
            Err(DialError::NoReply("timed out".into()))
        }
    }
}

/// Reply with `ctl` on a stream that has no link yet, then close with `code` after the peer
/// closed or `wait` passed.
pub(crate) async fn reply_and_close(conn: Connection, mut send: SendStream, ctl: &Ctl, code: u32, wait: Duration) {
    let _ = write_ctl(&mut send, ctl).await;
    let _ = send.finish();
    tokio::select! {
        _ = conn.closed() => {}
        _ = tokio::time::sleep(wait) => {}
    }
    close_conn(&conn, code, "");
}
