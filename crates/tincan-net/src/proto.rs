//! iroh protocol handlers for `tincan/server/1` and `tincan/dm/1`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::Duration;

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use tincan_core::wire::{Ctl, RejectCode};
use tokio::sync::Notify;

use crate::inner::Inner;
use crate::link::{self, FrameReader};
use crate::session::SessionCmd;

/// Time a dialer gets to open the control stream and send its first frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(4);
/// Connections from known peers (members of one of my servers, DM contacts) that may wait
/// for their first frame at once (both ALPNs together). Unknown ids cannot take these.
pub(crate) const MAX_PREAUTH: usize = 64;
/// Connections from unknown ids (joiners, strangers) that may wait for their first frame at
/// once. When full, the oldest unknown waiter is dropped to make room (identities cost
/// nothing, so refusing newcomers would let one flooder lock everyone else out).
pub(crate) const MAX_PREAUTH_UNKNOWN: usize = 32;

/// Connections of one remote id that may wait for their first frame at once (a peer that
/// shares several servers with me dials them all at cold open).
pub(crate) const MAX_PREAUTH_PER_PEER: usize = 8;

/// Bounds connections that have not proven anything yet: known peers have their own
/// `MAX_PREAUTH` slots, unknown ids share `MAX_PREAUTH_UNKNOWN` (oldest evicted first), and
/// any one id holds at most `MAX_PREAUTH_PER_PEER`.
#[derive(Debug, Default)]
pub(crate) struct PreAuth {
    st: parking_lot::Mutex<GateState>,
}

#[derive(Debug, Default)]
struct GateState {
    known: usize,
    /// Unknown waiters, oldest first: (slot id, its eviction signal).
    unknown: VecDeque<(u64, Arc<Notify>)>,
    peers: HashMap<iroh::EndpointId, usize>,
    next_id: u64,
}

/// A held pre-auth slot (released on drop).
pub(crate) struct PreAuthSlot<'a> {
    gate: &'a PreAuth,
    peer: iroh::EndpointId,
    id: u64,
    /// Fires when an unknown waiter is evicted (None for known peers).
    evicted: Option<Arc<Notify>>,
}

impl Drop for PreAuthSlot<'_> {
    fn drop(&mut self) {
        let mut st = self.gate.st.lock();
        if let Some(n) = st.peers.get_mut(&self.peer) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                st.peers.remove(&self.peer);
            }
        }
        if self.evicted.is_some() {
            st.unknown.retain(|(id, _)| *id != self.id);
        } else {
            st.known = st.known.saturating_sub(1);
        }
    }
}

impl PreAuth {
    /// A slot for a connection from `peer` (`known`: a member of one of my servers or a DM
    /// contact), or None when that pool is full (known) or this peer already has
    /// `MAX_PREAUTH_PER_PEER` waiting.
    pub fn enter_peer(&self, peer: iroh::EndpointId, known: bool) -> Option<PreAuthSlot<'_>> {
        let mut st = self.st.lock();
        if st.peers.get(&peer).is_some_and(|n| *n >= MAX_PREAUTH_PER_PEER) {
            return None;
        }
        st.next_id += 1;
        let id = st.next_id;
        let evicted = if known {
            if st.known >= MAX_PREAUTH {
                return None;
            }
            st.known += 1;
            None
        } else {
            if st.unknown.len() >= MAX_PREAUTH_UNKNOWN
                && let Some((_, oldest)) = st.unknown.pop_front()
            {
                oldest.notify_one();
            }
            let n = Arc::new(Notify::new());
            st.unknown.push_back((id, n.clone()));
            Some(n)
        };
        *st.peers.entry(peer).or_default() += 1;
        Some(PreAuthSlot { gate: self, peer, id, evicted })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> iroh::EndpointId { iroh::SecretKey::generate().public() }

    #[test]
    fn preauth_bounds_per_peer_and_overall() {
        let gate = PreAuth::default();
        let a = id();
        let held: Vec<_> = (0..MAX_PREAUTH_PER_PEER).map(|_| gate.enter_peer(a, true).expect("a slot")).collect();
        assert!(gate.enter_peer(a, true).is_none(), "one peer cannot hold more");
        assert!(gate.enter_peer(a, false).is_none(), "in either pool");
        drop(held);
        assert!(gate.enter_peer(a, true).is_some(), "slots come back");
        let mut many = Vec::new();
        for _ in 0..(MAX_PREAUTH * 2) {
            if let Some(s) = gate.enter_peer(id(), true) {
                many.push(s);
            }
        }
        assert_eq!(many.len(), MAX_PREAUTH);
    }

    #[tokio::test]
    async fn unknown_ids_cannot_lock_out_known_peers_or_newcomers() {
        let gate = PreAuth::default();
        // A flood of fresh identities fills the unknown pool many times over.
        let flood: Vec<_> = (0..MAX_PREAUTH_UNKNOWN * 3).filter_map(|_| gate.enter_peer(id(), false)).collect();
        assert_eq!(flood.len(), MAX_PREAUTH_UNKNOWN * 3, "newcomers always get in (the oldest is evicted)");
        let mut evicted = 0;
        for s in &flood {
            let n = s.evicted.clone().expect("unknown slot");
            if tokio::time::timeout(Duration::from_millis(1), n.notified()).await.is_ok() {
                evicted += 1;
            }
        }
        assert_eq!(evicted, MAX_PREAUTH_UNKNOWN * 2, "all but the newest pool-full were told to give up");
        assert_eq!(gate.st.lock().unknown.len(), MAX_PREAUTH_UNKNOWN);
        // Known peers still have every slot.
        let known: Vec<_> = (0..MAX_PREAUTH).filter_map(|_| gate.enter_peer(id(), true)).collect();
        assert_eq!(known.len(), MAX_PREAUTH);
        drop(flood);
        drop(known);
        let st = gate.st.lock();
        assert!(st.unknown.is_empty() && st.known == 0 && st.peers.is_empty(), "everything released");
    }
}

async fn first_frame(conn: &Connection) -> Option<(iroh::endpoint::SendStream, FrameReader, Ctl)> {
    let fut = async {
        let (send, recv) = conn.accept_bi().await.ok()?;
        let mut reader = FrameReader::new(recv);
        let first = reader.next().await.ok()?;
        Some((send, reader, first))
    };
    tokio::time::timeout(FIRST_FRAME_TIMEOUT, fut).await.ok().flatten()
}

/// `first_frame` inside a pre-auth slot (None: refused, evicted, or nothing valid in time).
async fn gated_first_frame(gate: &PreAuth, net: &Weak<Inner>, conn: &Connection) -> Option<(iroh::endpoint::SendStream, FrameReader, Ctl)> {
    let peer = crate::addr::remote_user(conn);
    let known = net.upgrade().is_some_and(|n| n.is_known_peer(&peer));
    let slot = gate.enter_peer(conn.remote_id(), known)?;
    match slot.evicted.clone() {
        Some(evicted) => tokio::select! {
            r = first_frame(conn) => r,
            _ = evicted.notified() => None,
        },
        None => first_frame(conn).await,
    }
}

/// Handler for `tincan/server/1`: routes the connection to the session of the server named
/// in the first frame.
#[derive(Debug, Clone)]
pub(crate) struct ServerProto {
    /// The network layer (weak: the router must not keep it alive).
    pub net: Weak<Inner>,
    /// Pre-auth bounds (shared with the DM handler).
    pub gate: Arc<PreAuth>,
}

impl ProtocolHandler for ServerProto {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let Some((send, reader, first)) = gated_first_frame(&self.gate, &self.net, &conn).await else {
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
    /// Pre-auth bounds (shared with the server handler).
    pub gate: Arc<PreAuth>,
}

impl ProtocolHandler for DmProto {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let Some((send, reader, first)) = gated_first_frame(&self.gate, &self.net, &conn).await else {
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
