//! iroh protocol handlers for `tincan/server/1` and `tincan/dm/1`.

use std::sync::{Arc, Weak};
use std::time::Duration;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use tincan_core::wire::{Ctl, RejectCode};

use crate::inner::Inner;
use crate::link::{self, FrameReader};
use crate::session::SessionCmd;

/// Time a dialer gets to open the control stream and send its first frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(4);

async fn first_frame(conn: &Connection) -> Option<(iroh::endpoint::SendStream, FrameReader, Ctl)> {
    let fut = async {
        let (send, recv) = conn.accept_bi().await.ok()?;
        let mut reader = FrameReader::new(recv);
        let first = reader.next().await.ok()?;
        Some((send, reader, first))
    };
    tokio::time::timeout(FIRST_FRAME_TIMEOUT, fut).await.ok().flatten()
}

/// Handler for `tincan/server/1`: routes the connection to the session of the server named
/// in the first frame.
#[derive(Debug, Clone)]
pub(crate) struct ServerProto {
    /// The network layer (weak: the router must not keep it alive).
    pub net: Weak<Inner>,
}

impl ProtocolHandler for ServerProto {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let Some((send, reader, first)) = first_frame(&conn).await else {
            link::close_conn(&conn, tincan_core::CLOSE_REJECTED, "no hello");
            return Ok(());
        };
        let Some(net) = self.net.upgrade() else {
            link::close_conn(&conn, tincan_core::CLOSE_NORMAL, "shutting down");
            return Ok(());
        };
        let server = match &first {
            Ctl::Hello { server, .. } | Ctl::JoinRequest { server, .. } => *server,
            _ => {
                let ctl = Ctl::Reject { code: RejectCode::Internal, reason: "expected Hello or JoinRequest".into() };
                link::reply_and_close(conn, send, &ctl, tincan_core::CLOSE_REJECTED, Duration::from_secs(1)).await;
                return Ok(());
            }
        };
        let tx = net.sessions.lock().get(&server).map(|s| s.tx.clone());
        match tx {
            Some(tx) => {
                let cmd = SessionCmd::Incoming { conn: conn.clone(), send, reader, first };
                if tx.send(cmd).await.is_err() {
                    link::close_conn(&conn, tincan_core::CLOSE_NORMAL, "session ended");
                }
            }
            None => {
                let ctl = Ctl::Reject { code: RejectCode::NotMember, reason: "unknown server".into() };
                link::reply_and_close(conn, send, &ctl, tincan_core::CLOSE_REJECTED, Duration::from_secs(1)).await;
            }
        }
        Ok(())
    }
}

/// Handler for `tincan/dm/1`.
#[derive(Debug, Clone)]
pub(crate) struct DmProto {
    /// The network layer.
    pub net: Weak<Inner>,
}

impl ProtocolHandler for DmProto {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let Some((send, reader, first)) = first_frame(&conn).await else {
            link::close_conn(&conn, tincan_core::CLOSE_REJECTED, "no hello");
            return Ok(());
        };
        let Some(net): Option<Arc<Inner>> = self.net.upgrade() else {
            link::close_conn(&conn, tincan_core::CLOSE_NORMAL, "shutting down");
            return Ok(());
        };
        crate::dm::accept_incoming(net, conn, send, reader, first).await;
        Ok(())
    }
}
