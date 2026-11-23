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
/// Connections from known peers (members of one of my servers, DM contacts I wrote to) that
/// may wait for their first frame at once (both ALPNs together). Unknown ids cannot take
/// these. When full, a waiter of the peer holding the most slots is dropped to make room
/// (honest first frames arrive within a round trip, so only idle holders lose theirs).
pub(crate) const MAX_PREAUTH: usize = 64;
/// Connections from unknown ids (joiners, strangers) that may wait for their first frame at
/// once. When full, the oldest unknown waiter is dropped to make room (identities cost
/// nothing, so refusing newcomers would let one flooder lock everyone else out).
pub(crate) const MAX_PREAUTH_UNKNOWN: usize = 32;

/// Connections of one remote id that may wait for their first frame at once (a peer that
/// shares several servers with me dials them all at cold open).
pub(crate) const MAX_PREAUTH_PER_PEER: usize = 8;

/// Bounds connections that have not proven anything yet: known peers have their own
/// `MAX_PREAUTH` slots, unknown ids share `MAX_PREAUTH_UNKNOWN`, any one id holds at most
/// `MAX_PREAUTH_PER_PEER`, and a full pool makes room instead of refusing.
#[derive(Debug, Default)]
pub(crate) struct PreAuth {
    st: parking_lot::Mutex<GateState>,
}

#[derive(Debug, Default)]
struct GateState {
    /// Known waiters, oldest first: (slot id, peer, its eviction signal).
    known: VecDeque<(u64, iroh::EndpointId, Arc<Notify>)>,
    /// Unknown waiters, oldest first: (slot id, its eviction signal).
    unknown: VecDeque<(u64, Arc<Notify>)>,
    peers: HashMap<iroh::EndpointId, usize>,
    next_id: u64,
}

impl GateState {
    /// Make room in the full known pool: the oldest waiter of the peer holding the most
    /// known slots (the oldest overall when nobody holds more than one) is told to give up.
    fn evict_known(&mut self) {
        let mut counts: HashMap<iroh::EndpointId, usize> = HashMap::new();
        for (_, p, _) in &self.known {
            *counts.entry(*p).or_default() += 1;
        }
        let most = counts.values().copied().max().unwrap_or(0);
        if let Some(pos) = self.known.iter().position(|(_, p, _)| counts.get(p).copied() == Some(most))
            && let Some((_, _, n)) = self.known.remove(pos)
        {
            n.notify_one();
        }
    }
}

/// A held pre-auth slot (released on drop).
pub(crate) struct PreAuthSlot<'a> {
    gate: &'a PreAuth,
    peer: iroh::EndpointId,
    id: u64,
    known: bool,
    /// Fires when this waiter is evicted to make room.
    evicted: Arc<Notify>,
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
        if self.known {
            st.known.retain(|(id, _, _)| *id != self.id);
        } else {
            st.unknown.retain(|(id, _)| *id != self.id);
        }
    }
}

impl PreAuth {
    /// A slot for a connection from `peer` (`known`: a member of one of my servers or a DM
    /// contact), or None when this peer already has `MAX_PREAUTH_PER_PEER` waiting. A full
    /// pool makes room by evicting a waiter.
    pub fn enter_peer(&self, peer: iroh::EndpointId, known: bool) -> Option<PreAuthSlot<'_>> {
        let mut st = self.st.lock();
        if st.peers.get(&peer).is_some_and(|n| *n >= MAX_PREAUTH_PER_PEER) {
            return None;
        }
        st.next_id += 1;
        let id = st.next_id;
        let n = Arc::new(Notify::new());
        if known {
            if st.known.len() >= MAX_PREAUTH {
                st.evict_known();
            }
            st.known.push_back((id, peer, n.clone()));
        } else {
            if st.unknown.len() >= MAX_PREAUTH_UNKNOWN
                && let Some((_, oldest)) = st.unknown.pop_front()
            {
                oldest.notify_one();
            }
            st.unknown.push_back((id, n.clone()));
        }
        *st.peers.entry(peer).or_default() += 1;
        Some(PreAuthSlot { gate: self, peer, id, known, evicted: n })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> iroh::EndpointId { iroh::SecretKey::generate().public() }

    async fn told_to_give_up(s: &PreAuthSlot<'_>) -> bool { tokio::time::timeout(Duration::from_millis(1), s.evicted.notified()).await.is_ok() }

    #[tokio::test]
    async fn preauth_bounds_per_peer_and_overall() {
        let gate = PreAuth::default();
        let a = id();
        let held: Vec<_> = (0..MAX_PREAUTH_PER_PEER).map(|_| gate.enter_peer(a, true).expect("a slot")).collect();
        assert!(gate.enter_peer(a, true).is_none(), "one peer cannot hold more");
        assert!(gate.enter_peer(a, false).is_none(), "in either pool");
        drop(held);
        assert!(gate.enter_peer(a, true).is_some(), "slots come back");
        // A full known pool makes room instead of refusing (it used to refuse, which let a
        // few known ids lock every member and contact out): at most MAX_PREAUTH wait.
        let mut many = Vec::new();
        for _ in 0..(MAX_PREAUTH * 2) {
            many.push(gate.enter_peer(id(), true).expect("a known peer always gets a slot"));
        }
        let mut evicted = 0;
        for s in &many {
            if told_to_give_up(s).await {
                evicted += 1;
            }
        }
        assert_eq!(evicted, MAX_PREAUTH, "the oldest waiters were told to give up");
        assert_eq!(gate.st.lock().known.len(), MAX_PREAUTH);
    }

    /// NET-SECURITY-4: eight known ids holding eight slots each (the whole known pool) do not
    /// lock out another member or contact: one of the flooders' slots is given up.
    #[tokio::test]
    async fn known_flooders_cannot_lock_out_known_peers() {
        let gate = PreAuth::default();
        let flooders: Vec<_> = (0..MAX_PREAUTH / MAX_PREAUTH_PER_PEER).map(|_| id()).collect();
        let held: Vec<_> = flooders.iter().flat_map(|f| (0..MAX_PREAUTH_PER_PEER).map(|_| gate.enter_peer(*f, true).expect("a slot"))).collect();
        assert_eq!(gate.st.lock().known.len(), MAX_PREAUTH);
        let member = id();
        let slot = gate.enter_peer(member, true).expect("a member still gets a slot");
        assert!(!told_to_give_up(&slot).await);
        let mut evicted = 0;
        for s in &held {
            if told_to_give_up(s).await {
                assert!(flooders.contains(&s.peer));
                evicted += 1;
            }
        }
        assert_eq!(evicted, 1, "exactly one flooder slot made room");
        // A second newcomer takes from a flooder that still holds the most, not the member.
        let second = gate.enter_peer(id(), true).expect("another slot");
        assert!(!told_to_give_up(&slot).await && !told_to_give_up(&second).await);
        drop(held);
        drop(slot);
        drop(second);
        let st = gate.st.lock();
        assert!(st.known.is_empty() && st.peers.is_empty(), "everything released");
    }

    #[tokio::test]
    async fn unknown_ids_cannot_lock_out_known_peers_or_newcomers() {
        let gate = PreAuth::default();
        // A flood of fresh identities fills the unknown pool many times over.
        let flood: Vec<_> = (0..MAX_PREAUTH_UNKNOWN * 3).filter_map(|_| gate.enter_peer(id(), false)).collect();
        assert_eq!(flood.len(), MAX_PREAUTH_UNKNOWN * 3, "newcomers always get in (the oldest is evicted)");
        let mut evicted = 0;
        for s in &flood {
            if told_to_give_up(s).await {
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
        assert!(st.unknown.is_empty() && st.known.is_empty() && st.peers.is_empty(), "everything released");
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
    let known = net.upgrade().is_some_and(|n| n.is_trusted_dialer(&peer));
    let slot = gate.enter_peer(conn.remote_id(), known)?;
    let evicted = slot.evicted.clone();
    tokio::select! {
        r = first_frame(conn) => r,
        _ = evicted.notified() => None,
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
