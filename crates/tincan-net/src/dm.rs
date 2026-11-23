//! Direct messages: one `tincan/dm/1` connection per contact, opened on demand.
//!
//! Both sides store the DM log (same `Event` format, conv `ConvId::dm(a, b)`). On connect
//! both send `DmHello{conv, heads, have}` (dialer first) and stream what the other lacks.
//! Live events go as `Events{[ev]}`; the receiver acknowledges with `SyncHave{heads}` after
//! applying something, which tells the sender how far its events were delivered
//! (`NetEvent::DmDelivered`, persisted in kv). Undelivered events are retried on a backoff
//! (30 s, 1, 2, 5, 15 min, then every 15 min) and whenever the contact shows up in a roster
//! or an address for it is learned.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::{Connection, SendStream};
use parking_lot::Mutex;
use tincan_core::event::{Body, StoredEvent};
use tincan_core::state::{ConvState, Verdict, dm_channel};
use tincan_core::storage::ConvRecord;
use tincan_core::sync::Heads;
use tincan_core::wire::Ctl;
use tincan_core::{ALPN_DM, BlobHash, ConvId, UserId, build, now_ms};
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::error::{NetError, Result};
use crate::events::NetEvent;
use crate::hooks::{CtlSource, is_extra};
use crate::inner::Inner;
use crate::limits::{Bucket, Buckets};
use crate::link::{self, Dialed, FrameReader, Link, LinkEvent};
use crate::replica::{Replica, ReplicaRef, send_missing};

fn ack_key(conv: &ConvId) -> String { format!("net/dm_ack/{}", conv.to_hex()) }

/// The live connection to a contact.
#[derive(Debug)]
struct DmLink {
    id: u64,
    dialer: UserId,
    conn: Connection,
    live: mpsc::Sender<Bytes>,
}

/// Per-contact DM state.
#[derive(Debug, Default)]
struct DmPeer {
    link: Option<DmLink>,
    dialing: bool,
    failures: usize,
    retry: Option<AbortHandle>,
    ack: u64,
    ack_loaded: bool,
    holds: usize,
}

/// Stranger DM connections (no stored conversation, no shared server) held at once.
pub(crate) const MAX_STRANGER_LINKS: usize = 8;
/// New stranger DM connections: burst, then one per this (10 per minute).
const STRANGER_EVERY: Duration = Duration::from_secs(6);
const STRANGER_BURST: u32 = 10;
/// Conversations opened by strangers that I never answered, kept at once. Beyond this, a
/// stranger's first message is not accepted (no row, no `DmOpened`).
pub(crate) const MAX_UNANSWERED_STRANGER_CONVS: usize = 32;
/// Typing notices per contact: one per this.
const DM_TYPING_EVERY: Duration = Duration::from_secs(2);
/// Rings per contact: a burst of 5, then one per 10 s.
const CALL_INVITE_EVERY: Duration = Duration::from_secs(10);
const CALL_INVITE_BURST: u32 = 5;
/// Call state updates per contact.
const CALL_STATE_EVERY: Duration = Duration::from_millis(500);
const CALL_STATE_BURST: u32 = 4;

/// Per-contact budgets of what a DM peer can make this device emit.
#[derive(Debug)]
struct DmGates {
    strangers: Bucket,
    typing: Buckets<UserId>,
    call_invite: Buckets<UserId>,
    call_state: Buckets<UserId>,
}

impl Default for DmGates {
    fn default() -> Self {
        Self {
            strangers: Bucket::every(STRANGER_EVERY, STRANGER_BURST, Instant::now()),
            typing: Buckets::new(DM_TYPING_EVERY, 1),
            call_invite: Buckets::new(CALL_INVITE_EVERY, CALL_INVITE_BURST),
            call_state: Buckets::new(CALL_STATE_EVERY, CALL_STATE_BURST),
        }
    }
}

/// All DM connections and delivery state.
#[derive(Debug)]
pub(crate) struct DmManager {
    peers: Mutex<HashMap<UserId, DmPeer>>,
    next_id: Mutex<u64>,
    changed: watch::Sender<u64>,
    /// Peers whose connection was accepted while they were strangers (no conversation yet).
    strangers: Mutex<std::collections::HashSet<UserId>>,
    gates: Mutex<DmGates>,
}

impl Default for DmManager {
    fn default() -> Self {
        Self {
            peers: Mutex::new(HashMap::new()),
            next_id: Mutex::new(0),
            changed: watch::channel(0).0,
            strangers: Mutex::new(std::collections::HashSet::new()),
            gates: Mutex::new(DmGates::default()),
        }
    }
}

/// Keeps the DM connection to a contact open while held (part B: calls).
#[derive(Debug)]
pub struct DmHold {
    net: std::sync::Weak<Inner>,
    peer: UserId,
}

impl Drop for DmHold {
    fn drop(&mut self) {
        if let Some(net) = self.net.upgrade() {
            let mut peers = net.dm.peers.lock();
            if let Some(p) = peers.get_mut(&self.peer) {
                p.holds = p.holds.saturating_sub(1);
            }
        }
    }
}

impl DmManager {
    fn bump(&self) { self.changed.send_modify(|v| *v = v.wrapping_add(1)); }

    /// Is there a live DM connection to `user`?
    pub fn is_connected(&self, user: &UserId) -> bool { self.peers.lock().get(user).is_some_and(|p| p.link.is_some()) }

    /// The DM connection to `user`, if up.
    pub fn connection(&self, user: &UserId) -> Option<Connection> {
        self.peers.lock().get(user).and_then(|p| p.link.as_ref().map(|l| l.conn.clone()))
    }

    /// Highest own seq the peer confirmed.
    pub fn delivered(&self, net: &Inner, conv: &ConvId, peer: &UserId) -> u64 { self.ack(net, conv, peer) }

    fn ack(&self, net: &Inner, conv: &ConvId, peer: &UserId) -> u64 {
        let mut peers = self.peers.lock();
        let p = peers.entry(*peer).or_default();
        if !p.ack_loaded {
            p.ack_loaded = true;
            p.ack = match net.storage.get_kv(&ack_key(conv)) {
                Ok(Some(v)) if v.len() == 8 => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&v);
                    u64::from_le_bytes(b)
                }
                _ => 0,
            };
        }
        p.ack
    }

    /// My head in the DM with `peer`.
    fn my_head(net: &Inner, conv: &ConvId) -> u64 { net.replica(conv).map(|r| r.lock().state.head(&net.me)).unwrap_or(0) }

    /// Do I have events `peer` has not confirmed?
    pub fn has_pending(&self, net: &Inner, peer: &UserId) -> bool {
        let conv = ConvId::dm(&net.me, peer);
        Self::my_head(net, &conv) > self.ack(net, &conv, peer)
    }

    /// Record the peer's heads: update and persist the delivery ack.
    fn note_heads(&self, net: &Inner, conv: &ConvId, peer: &UserId, heads: &Heads) {
        let theirs = heads.to_map().get(&net.me).copied().unwrap_or(0);
        let current = self.ack(net, conv, peer);
        if theirs > current {
            if let Some(p) = self.peers.lock().get_mut(peer) {
                p.ack = theirs;
            }
            if let Err(e) = net.storage.set_kv(&ack_key(conv), &theirs.to_le_bytes()) {
                tracing::warn!(error = %e, "persisting DM ack failed");
            }
            net.emit(NetEvent::DmDelivered { conv: *conv, up_to_seq: theirs });
        }
    }

    /// Contacts seen online somewhere: deliver pending DMs now.
    pub fn peers_seen(&self, net: &Arc<Inner>, users: &[UserId]) {
        for u in users {
            let known = self.peers.lock().contains_key(u) || net.replica(&ConvId::dm(&net.me, u)).is_some();
            if known && self.has_pending(net, u) {
                self.ensure_connect(net, *u);
            }
        }
    }

    /// Hold the connection open (calls). The connection is not opened by this call.
    pub fn hold(&self, net: &Arc<Inner>, peer: UserId) -> DmHold {
        self.peers.lock().entry(peer).or_default().holds += 1;
        DmHold { net: Arc::downgrade(net), peer }
    }

    /// Dial `peer` unless connected or dialing.
    pub fn ensure_connect(&self, net: &Arc<Inner>, peer: UserId) {
        if peer == net.me || net.closed.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        {
            let mut peers = self.peers.lock();
            let p = peers.entry(peer).or_default();
            if p.link.is_some() || p.dialing {
                return;
            }
            p.dialing = true;
            if let Some(r) = p.retry.take() {
                r.abort();
            }
        }
        let net2 = net.clone();
        net.tasks.spawn(async move {
            let res = dial(&net2, peer).await;
            match res {
                Ok(d) => net2.dm.register(&net2, peer, d, net2.me),
                Err(e) => {
                    tracing::debug!(peer = %peer.short(), error = %e, "DM dial failed");
                    net2.dm.dial_failed(&net2, peer);
                }
            }
        });
    }

    fn dial_failed(&self, net: &Arc<Inner>, peer: UserId) {
        let pending = self.has_pending(net, &peer);
        let mut peers = self.peers.lock();
        let p = peers.entry(peer).or_default();
        p.dialing = false;
        if !pending {
            p.failures = 0;
            return;
        }
        let delay = net.timings.dm_retry_delay(p.failures);
        p.failures += 1;
        let net2 = net.clone();
        let h = net.tasks.spawn(async move {
            tokio::time::sleep(delay).await;
            if let Some(p) = net2.dm.peers.lock().get_mut(&peer) {
                p.retry = None;
            }
            net2.dm.ensure_connect(&net2, peer);
        });
        if let Some(old) = p.retry.replace(h) {
            old.abort();
        }
    }

    /// Wait until a DM connection to `peer` is up (dialing if needed), up to the dial timeout.
    pub async fn connect(&self, net: &Arc<Inner>, peer: UserId) -> Result<Connection> {
        let mut rx = self.changed.subscribe();
        self.ensure_connect(net, peer);
        let deadline = Instant::now() + net.timings.dial + net.timings.dial;
        loop {
            if let Some(c) = self.connection(&peer) {
                return Ok(c);
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(NetError::Closed),
                Err(_) => return Err(NetError::Unreachable("Couldn't reach them right now.".into())),
            }
        }
    }

    /// The DM connection to `peer` and its control link's live queue (calls ride on it).
    pub fn live(&self, peer: &UserId) -> Option<(Connection, mpsc::Sender<Bytes>)> {
        self.peers.lock().get(peer).and_then(|p| p.link.as_ref().map(|l| (l.conn.clone(), l.live.clone())))
    }

    /// Queue a frame on the DM connection to `peer`.
    pub fn send(&self, peer: &UserId, ctl: &Ctl) -> bool {
        let peers = self.peers.lock();
        let Some(l) = peers.get(peer).and_then(|p| p.link.as_ref()) else { return false };
        match link::encode(ctl) {
            Some(b) => l.live.try_send(b).is_ok(),
            None => false,
        }
    }

    /// Send a Have delta on every DM connection.
    pub fn broadcast_have(&self, add: &[BlobHash], remove: &[BlobHash]) {
        for ctl in tincan_core::wire::have_frames(add, remove) {
            let Some(b) = link::encode(&ctl) else { continue };
            for p in self.peers.lock().values() {
                if let Some(l) = &p.link {
                    let _ = l.live.try_send(b.clone());
                }
            }
        }
    }

    /// Close every DM connection.
    pub fn close_all(&self) {
        let mut peers = self.peers.lock();
        for p in peers.values_mut() {
            if let Some(r) = p.retry.take() {
                r.abort();
            }
            if let Some(l) = p.link.take() {
                link::close_conn(&l.conn, tincan_core::CLOSE_NORMAL, "bye");
            }
        }
    }

    /// Install a connection whose DmHello exchange is done.
    fn register(&self, net: &Arc<Inner>, peer: UserId, d: Dialed, dialer: UserId) {
        let Ctl::DmHello { conv, heads, .. } = d.reply.clone() else {
            link::close_conn(&d.conn, tincan_core::CLOSE_REJECTED, "expected DmHello");
            self.peers.lock().entry(peer).or_default().dialing = false;
            return;
        };
        if conv != ConvId::dm(&net.me, &peer) {
            link::close_conn(&d.conn, tincan_core::CLOSE_REJECTED, "wrong conversation");
            self.peers.lock().entry(peer).or_default().dialing = false;
            return;
        }
        let id = {
            let mut n = self.next_id.lock();
            *n += 1;
            *n
        };
        let (tx, rx) = mpsc::channel(256);
        let sync_tx = tx.clone();
        let new_link = Link::spawn(&net.tasks, id, d.conn.clone(), d.send, d.reader, tx);
        let keep_new = {
            let mut peers = self.peers.lock();
            let p = peers.entry(peer).or_default();
            if dialer == net.me {
                p.dialing = false;
            }
            // Simultaneous dials: keep the connection dialed by the lower user id.
            p.link.as_ref().is_none_or(|existing| dialer <= existing.dialer)
        };
        if !keep_new {
            new_link.close(tincan_core::CLOSE_NORMAL, "duplicate");
            return;
        }
        {
            let mut peers = self.peers.lock();
            let p = peers.entry(peer).or_default();
            if let Some(existing) = &p.link {
                link::close_conn(&existing.conn, tincan_core::CLOSE_NORMAL, "duplicate");
            }
            p.link = Some(DmLink { id, dialer, conn: d.conn.clone(), live: new_link.live() });
            p.failures = 0;
            if let Some(r) = p.retry.take() {
                r.abort();
            }
        }
        self.bump();
        net.media.dm_link(net, peer);
        net.emit(NetEvent::DmLinkUp { peer });
        tracing::debug!(peer = %peer.short(), "DM connection up");
        let net2 = net.clone();
        net.tasks.spawn(run_conn(net2, peer, conv, new_link, rx, sync_tx, heads));
    }

    fn unregister(&self, net: &Arc<Inner>, peer: UserId, id: u64) {
        let removed = {
            let mut peers = self.peers.lock();
            match peers.get_mut(&peer) {
                Some(p) if p.link.as_ref().is_some_and(|l| l.id == id) => {
                    p.link = None;
                    true
                }
                _ => false,
            }
        };
        if removed {
            self.strangers.lock().remove(&peer);
            self.bump();
            net.media.dm_link(net, peer);
            net.emit(NetEvent::DmLinkDown { peer });
            if self.has_pending(net, &peer) {
                self.dial_failed(net, peer);
            }
            // A stranger that left nothing behind is forgotten.
            let conv = ConvId::dm(&net.me, &peer);
            if net.replica(&conv).is_none() {
                let mut peers = self.peers.lock();
                if peers.get(&peer).is_some_and(|p| p.link.is_none() && p.holds == 0 && p.retry.is_none() && !p.dialing) {
                    peers.remove(&peer);
                }
            }
        }
    }

    /// May a stranger's DM connection be accepted now? (A global rate and a cap on stranger
    /// connections held at once; one per peer.)
    fn admit_stranger(&self, peer: UserId) -> bool {
        let mut s = self.strangers.lock();
        if s.contains(&peer) {
            return true;
        }
        if s.len() >= MAX_STRANGER_LINKS || !self.gates.lock().strangers.take(Instant::now()) {
            return false;
        }
        s.insert(peer);
        true
    }

    /// Budget check for something a DM peer makes this device show (typing, rings, state).
    fn allow(&self, peer: &UserId, ctl: &Ctl) -> bool {
        let now = Instant::now();
        let mut g = self.gates.lock();
        match ctl {
            Ctl::Typing { .. } => g.typing.take(peer, now),
            Ctl::CallInvite { .. } => g.call_invite.take(peer, now),
            Ctl::CallState { .. } => g.call_state.take(peer, now),
            _ => true,
        }
    }
}

/// Conversations opened by strangers I never wrote in (each one a DM list entry and a row).
fn unanswered_stranger_convs(net: &Inner) -> usize {
    let dms: Vec<(UserId, ReplicaRef)> = net
        .replicas
        .read()
        .values()
        .filter_map(|r| {
            let g = r.lock();
            (g.state.is_dm).then(|| g.record.peer.map(|p| (p, r.clone()))).flatten()
        })
        .collect();
    dms.iter().filter(|(p, r)| r.lock().state.head(&net.me) == 0 && !net.shares_server(p)).count()
}

/// A stranger's batch: does its first event apply in order to an empty DM log (a real,
/// signed first message from them)? Nothing is stored and nothing is buffered here.
fn stranger_opens(net: &Inner, conv: &ConvId, peer: &UserId, events: &[tincan_core::event::SignedEvent]) -> bool {
    let mut state = ConvState::new_dm(net.me, *peer);
    for signed in events.iter().take(4) {
        let Ok(stored) = StoredEvent::from_signed(signed.clone()) else { continue };
        if stored.conv() != *conv || stored.event.author != *peer {
            continue;
        }
        if state.apply(&stored, now_ms()).0 == Verdict::Accept {
            return true;
        }
    }
    false
}

/// Make sure the DM record and replica for `peer` exist. Returns the conv and whether it
/// was created.
pub(crate) fn open_conv(net: &Arc<Inner>, peer: UserId) -> Result<(ConvId, bool)> {
    let conv = ConvId::dm(&net.me, &peer);
    if net.replica(&conv).is_some() {
        return Ok((conv, false));
    }
    let name = net.name_from_servers(&peer).unwrap_or_else(|| peer.to_z32().chars().take(8).collect());
    let record = ConvRecord {
        id: conv,
        is_dm: true,
        peer: Some(peer),
        name,
        server_secret: None,
        last_epoch: 0,
        last_host: None,
        joined_ms: now_ms(),
    };
    let replica = match net.storage.conv(&conv)? {
        Some(existing) => Replica::load(net.storage.as_ref(), existing, &net.me)?,
        None => {
            net.storage.upsert_conv(&record)?;
            Replica::new(net.storage.as_ref(), record, ConvState::new_dm(net.me, peer))
        }
    };
    let created = match net.replicas.write().entry(conv) {
        std::collections::hash_map::Entry::Occupied(_) => false,
        std::collections::hash_map::Entry::Vacant(v) => {
            v.insert(Arc::new(Mutex::new(replica)));
            true
        }
    };
    if created {
        net.emit(NetEvent::DmOpened { conv, peer });
    }
    Ok((conv, created))
}

/// The other party of a DM conversation.
pub(crate) fn peer_of(net: &Inner, conv: &ConvId) -> Option<UserId> {
    let r = net.replica(conv)?;
    let r = r.lock();
    if !r.state.is_dm {
        return None;
    }
    r.record.peer
}

/// Author a DM event, store it, and deliver it if connected (dial otherwise).
pub(crate) fn publish(net: &Arc<Inner>, conv: ConvId, body: Body) -> Result<StoredEvent> {
    let peer = peer_of(net, &conv).ok_or(NetError::UnknownConv)?;
    let replica = net.replica(&conv).ok_or(NetError::UnknownConv)?;
    let body = match body {
        Body::Message { text, attachments, .. } => Body::Message { channel: dm_channel(&conv), text, attachments },
        b @ Body::Delete { .. } => b,
        _ => return Err(NetError::Invalid("Only messages and deletions are possible in direct messages.".into())),
    };
    let stored = {
        let mut r = replica.lock();
        let seq = r.next_own_seq(&net.me);
        let hlc = net.hlc.now();
        let signed = build::event(&net.identity, conv, seq, hlc, body).map_err(NetError::from_core_user)?;
        let stored = StoredEvent::from_signed(signed)?;
        match r.state.check(&stored.id, &stored.event, now_ms()) {
            Verdict::Accept => {}
            Verdict::Reject(why) => return Err(NetError::Invalid(crate::error::capitalize(&why))),
            _ => return Err(NetError::Invalid("That is not possible yet.".into())),
        }
        r.reserve_own_seq(net.storage.as_ref(), seq);
        let res = r.ingest(net.storage.as_ref(), &net.hlc, stored.clone());
        drop(r);
        net.emit_applied(conv, &res.applied);
        stored
    };
    if !net.dm.send(&peer, &Ctl::Events { events: vec![stored.signed.clone()], more: false }) {
        net.dm.ensure_connect(net, peer);
    }
    Ok(stored)
}

async fn dial(net: &Arc<Inner>, peer: UserId) -> std::result::Result<Dialed, link::DialError> {
    let (conv, _) = open_conv(net, peer).map_err(|e| link::DialError::Unreachable(e.to_string()))?;
    let hint = net.hint(&peer);
    let conn = link::connect(&net.endpoint, &peer, hint.as_ref(), ALPN_DM, net.timings.dial).await?;
    let hello = dm_hello(net, &conv);
    link::exchange(conn, &hello, net.timings.dial).await
}

fn dm_hello(net: &Inner, conv: &ConvId) -> Ctl {
    let heads = net.replica(conv).map(|r| r.lock().state.sync_heads()).unwrap_or_default();
    Ctl::DmHello { conv: *conv, heads, have: net.have_list() }
}

/// Accept an incoming DM connection whose first frame was `first` (a DmHello).
pub(crate) async fn accept_incoming(net: Arc<Inner>, conn: Connection, mut send: SendStream, reader: FrameReader, first: Ctl) {
    let peer = crate::addr::remote_user(&conn);
    let Ctl::DmHello { conv, .. } = &first else {
        link::close_conn(&conn, tincan_core::CLOSE_REJECTED, "expected DmHello");
        return;
    };
    if *conv != ConvId::dm(&net.me, &peer) || peer == net.me {
        link::close_conn(&conn, tincan_core::CLOSE_REJECTED, "wrong conversation");
        return;
    }
    if net.is_known_peer(&peer) {
        if let Err(e) = open_conv(&net, peer) {
            tracing::warn!(error = %e, "cannot open DM conversation");
            link::close_conn(&conn, tincan_core::CLOSE_REJECTED, "internal");
            return;
        }
    } else if !net.dm.admit_stranger(peer) {
        // A stranger (no conversation, no shared server): accepted only within the limits,
        // and nothing is stored until they send a valid first message.
        link::close_conn(&conn, tincan_core::CLOSE_REJECTED, "busy");
        return;
    }
    let hello = dm_hello(&net, conv);
    if link::write_ctl(&mut send, &hello).await.is_err() {
        return;
    }
    let d = Dialed { conn, send, reader, reply: first };
    net.dm.register(&net, peer, d, peer);
}

/// History sync on one DM connection: at most one `send_missing` in flight, the latest
/// request queued (a peer repeating SyncHave or DmHello cannot start unbounded tasks).
struct DmSync {
    running: bool,
    queued: Option<Heads>,
    done: mpsc::Sender<LinkEvent>,
}

impl DmSync {
    fn request(&mut self, net: &Arc<Inner>, replica: &ReplicaRef, link: &Link, heads: Heads) {
        if self.running {
            self.queued = Some(heads);
            return;
        }
        if replica.lock().state.sync_heads().missing_in(&heads).is_empty() {
            return;
        }
        self.running = true;
        let storage = net.storage.clone();
        let rep = replica.clone();
        let bulk = link.bulk();
        let done = self.done.clone();
        let id = link.id;
        net.tasks.spawn(async move {
            send_missing(storage, rep, heads, bulk).await;
            let _ = done.send(LinkEvent::SyncSent { link: id }).await;
        });
    }

    fn finished(&mut self, net: &Arc<Inner>, replica: &ReplicaRef, link: &Link) {
        self.running = false;
        if let Some(h) = self.queued.take() {
            self.request(net, replica, link, h);
        }
    }
}

/// Drive one DM connection until it closes or goes idle. A stranger's connection runs
/// without a conversation until their first valid message opens one.
async fn run_conn(
    net: Arc<Inner>,
    peer: UserId,
    conv: ConvId,
    link: Link,
    mut rx: mpsc::Receiver<LinkEvent>,
    sync_tx: mpsc::Sender<LinkEvent>,
    first_heads: Heads,
) {
    let mut replica: Option<ReplicaRef> = net.replica(&conv);
    let mut sync = DmSync { running: false, queued: None, done: sync_tx };
    if let Some(r) = &replica {
        on_heads(&net, &peer, &conv, r, &link, &mut sync, first_heads);
    }
    let idle = net.timings.dm_idle;
    let mut idle_at = Instant::now() + idle;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(LinkEvent::Frame { ctl, .. }) => {
                    idle_at = Instant::now() + idle;
                    let ctl = *ctl;
                    if replica.is_none() {
                        replica = stranger_frame(&net, &peer, &conv, &ctl);
                        if replica.is_none() {
                            if is_extra(&ctl) && net.dm.allow(&peer, &ctl) {
                                net.extra(CtlSource::Dm { peer }, ctl);
                            }
                            continue;
                        }
                    }
                    if let Some(r) = &replica {
                        on_frame(&net, &peer, &conv, r, &link, &mut sync, ctl);
                    }
                }
                Some(LinkEvent::SyncSent { .. }) => {
                    if let Some(r) = &replica {
                        sync.finished(&net, r, &link);
                    }
                }
                Some(LinkEvent::Closed { .. }) | None => break,
            },
            _ = tokio::time::sleep_until(idle_at) => {
                let held = net.dm.peers.lock().get(&peer).is_some_and(|p| p.holds > 0);
                if held || net.dm.has_pending(&net, &peer) {
                    idle_at = Instant::now() + idle;
                } else {
                    tracing::debug!(peer = %peer.short(), "closing idle DM connection");
                    link.close(tincan_core::CLOSE_NORMAL, "idle");
                    break;
                }
            }
        }
    }
    net.dm.unregister(&net, peer, link.id);
}

fn on_heads(net: &Arc<Inner>, peer: &UserId, conv: &ConvId, replica: &ReplicaRef, link: &Link, sync: &mut DmSync, heads: Heads) {
    net.dm.note_heads(net, conv, peer, &heads);
    sync.request(net, replica, link, heads);
}

/// A frame from a stranger whose conversation does not exist yet. Returns the replica if
/// this frame opened the conversation (it is then handled normally).
fn stranger_frame(net: &Arc<Inner>, peer: &UserId, conv: &ConvId, ctl: &Ctl) -> Option<ReplicaRef> {
    let Ctl::Events { events, .. } = ctl else { return None };
    if !stranger_opens(net, conv, peer, events) {
        // Early or invalid events from a stranger are never buffered.
        return None;
    }
    if net.replica(conv).is_none() && unanswered_stranger_convs(net) >= MAX_UNANSWERED_STRANGER_CONVS {
        tracing::info!(peer = %peer.short(), "too many unanswered stranger conversations; ignoring a new one");
        return None;
    }
    match open_conv(net, *peer) {
        Ok(_) => {
            net.dm.strangers.lock().remove(peer);
            net.replica(conv)
        }
        Err(e) => {
            tracing::warn!(error = %e, "cannot open DM conversation");
            None
        }
    }
}

fn on_frame(net: &Arc<Inner>, peer: &UserId, conv: &ConvId, replica: &ReplicaRef, link: &Link, sync: &mut DmSync, ctl: Ctl) {
    match ctl {
        Ctl::Events { events, .. } => {
            if events.is_empty() {
                return;
            }
            let mut applied = Vec::new();
            {
                let mut r = replica.lock();
                for signed in events {
                    let Ok(stored) = StoredEvent::from_signed(signed) else { continue };
                    if stored.conv() != *conv {
                        continue;
                    }
                    applied.extend(r.ingest(net.storage.as_ref(), &net.hlc, stored).applied);
                }
            }
            if !applied.is_empty() {
                net.emit_applied(*conv, &applied);
                let heads = replica.lock().state.sync_heads();
                let _ = link.send(&Ctl::SyncHave { heads });
            }
        }
        Ctl::SyncHave { heads } => on_heads(net, peer, conv, replica, link, sync, heads),
        Ctl::Have { .. } => {}
        Ctl::Typing { by, .. } => {
            if by == *peer && net.dm.allow(peer, &ctl) {
                net.emit(NetEvent::Typing { conv: *conv, channel: dm_channel(conv), user: by });
            }
        }
        Ctl::DmHello { heads, .. } => on_heads(net, peer, conv, replica, link, sync, heads),
        other if is_extra(&other) => {
            if net.dm.allow(peer, &other) {
                net.extra(CtlSource::Dm { peer: *peer }, other);
            }
        }
        other => tracing::trace!(?other, "DM: ignoring frame"),
    }
}
