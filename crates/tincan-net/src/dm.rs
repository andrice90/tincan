//! Direct messages: one `tincan/dm/1` connection per contact, opened on demand.
//!
//! Both sides store the DM log (same `Event` format, conv `ConvId::dm(a, b)`). On connect
//! both send `DmHello{conv, heads, have}` (dialer first) and stream what the other lacks.
//! Live events go as `Events{[ev]}`; the receiver acknowledges with `SyncHave{heads}` after
//! applying something, which tells the sender how far its events were delivered
//! (`NetEvent::DmDelivered`, persisted in kv). Undelivered events are retried on a backoff
//! (30 s, 1, 2, 5, 15 min, then every 15 min) and whenever the contact shows up in a roster
//! or an address for it is learned. A dial asked for while one is in flight goes out at once
//! when that dial fails and the contact's address changed meanwhile.
//!
//! Once a connection is up both sides send `DmProfile{name}` (their display name, again when
//! it changes). A watched contact (`Net::dm_watch`) stays connected: a lost or failed
//! connection is redialed on a short backoff, and a `Tick` every `dm_keepalive` keeps the
//! contact's side from closing it as idle (every version ignores a `Tick` on a DM link).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::{Connection, SendStream};
use parking_lot::Mutex;
use tincan_core::event::{Body, StoredEvent};
use tincan_core::state::{ConvState, Verdict, dm_channel, validate_name};
use tincan_core::storage::ConvRecord;
use tincan_core::sync::Heads;
use tincan_core::wire::{AddrHint, Ctl};
use tincan_core::{ALPN_DM, ConvId, UserId, build, now_ms};
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

fn name_key(conv: &ConvId) -> String { format!("net/dm_name/{}", conv.to_hex()) }

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
    /// The address the dial in flight started with.
    dial_hint: Option<AddrHint>,
    /// A dial was asked for while one was in flight.
    redial: bool,
    failures: usize,
    retry: Option<AbortHandle>,
    ack: u64,
    ack_loaded: bool,
    holds: usize,
    /// `DmWatch` guards alive.
    watches: usize,
    /// The display name the contact announced (validated), once kv was read.
    name: Option<String>,
    name_loaded: bool,
    /// Reachability probes that failed in a row, and when the last one failed.
    probe_failures: usize,
    probe_failed_at: Option<Instant>,
}

/// Stranger DM connections (no stored conversation, no shared server) held at once.
pub(crate) const MAX_STRANGER_LINKS: usize = 8;
/// New stranger DM connections: burst, then one per this (10 per minute).
const STRANGER_EVERY: Duration = Duration::from_secs(6);
const STRANGER_BURST: u32 = 10;
/// Conversations opened by strangers that I never answered, kept at once. Beyond this, a
/// stranger's first message is not accepted (no row, no `DmOpened`).
pub(crate) const MAX_UNANSWERED_STRANGER_CONVS: usize = 32;
/// An unanswered stranger conversation older than this no longer counts toward
/// `MAX_UNANSWERED_STRANGER_CONVS` (it stays stored).
const STALE_STRANGER_CONV_MS: u64 = 3 * 24 * 60 * 60 * 1000;
/// New events applied per contact: 2000 in a row (a backlog after a long absence), then 5
/// per second. Over it the rest of the page is not applied and is asked for again later.
const DM_EVENTS_EVERY: Duration = Duration::from_millis(200);
const DM_EVENTS_BURST: u32 = 2000;
/// The same while I never wrote in the conversation and we share no server (a stranger's
/// conversation): 50 in a row, then one per minute.
const DM_UNANSWERED_EVERY: Duration = Duration::from_secs(60);
const DM_UNANSWERED_BURST: u32 = 50;
/// Bytes of new events applied per contact: 8 MiB in a row, then 16 KiB/s.
const DM_BYTES_PER_S: f64 = 16.0 * 1024.0;
const DM_BYTES_BURST: f64 = 8.0 * 1024.0 * 1024.0;
/// After events were cut, what is missing is asked for again at most this often, once the
/// budget has room.
const DM_REFETCH_EVERY: Duration = Duration::from_secs(1);
/// Typing notices per contact: one per this.
const DM_TYPING_EVERY: Duration = Duration::from_secs(2);
/// Rings per contact: a burst of 5, then one per 10 s.
const CALL_INVITE_EVERY: Duration = Duration::from_secs(10);
const CALL_INVITE_BURST: u32 = 5;
/// Call state updates per contact.
const CALL_STATE_EVERY: Duration = Duration::from_millis(500);
const CALL_STATE_BURST: u32 = 4;
/// Name changes per contact that are stored and announced: a burst of 3, then one per 5 s.
const DM_PROFILE_EVERY: Duration = Duration::from_secs(5);
const DM_PROFILE_BURST: u32 = 3;

/// Per-contact budgets of what a DM peer can make this device emit.
#[derive(Debug)]
struct DmGates {
    strangers: Bucket,
    typing: Buckets<UserId>,
    call_invite: Buckets<UserId>,
    call_state: Buckets<UserId>,
    profile: Buckets<UserId>,
    /// New events applied per contact (`DM_EVENTS_BURST`).
    events: Buckets<UserId>,
    /// New events applied in a stranger's conversation (`DM_UNANSWERED_BURST`).
    unanswered: Buckets<UserId>,
    /// Bytes of new events applied per contact.
    event_bytes: Buckets<UserId>,
}

impl Default for DmGates {
    fn default() -> Self {
        Self {
            strangers: Bucket::every(STRANGER_EVERY, STRANGER_BURST, Instant::now()),
            typing: Buckets::new(DM_TYPING_EVERY, 1),
            call_invite: Buckets::new(CALL_INVITE_EVERY, CALL_INVITE_BURST),
            call_state: Buckets::new(CALL_STATE_EVERY, CALL_STATE_BURST),
            profile: Buckets::new(DM_PROFILE_EVERY, DM_PROFILE_BURST),
            events: Buckets::new(DM_EVENTS_EVERY, DM_EVENTS_BURST),
            unanswered: Buckets::new(DM_UNANSWERED_EVERY, DM_UNANSWERED_BURST),
            event_bytes: Buckets::rate(DM_BYTES_PER_S, DM_BYTES_BURST),
        }
    }
}

/// All DM connections and delivery state.
#[derive(Debug)]
pub(crate) struct DmManager {
    peers: Mutex<HashMap<UserId, DmPeer>>,
    next_id: Mutex<u64>,
    changed: watch::Sender<u64>,
    /// Peers whose connection was accepted while they were strangers (no conversation yet),
    /// with the ticket of the slot and when it was taken.
    strangers: Mutex<HashMap<UserId, (u64, Instant)>>,
    next_ticket: std::sync::atomic::AtomicU64,
    gates: Mutex<DmGates>,
}

impl Default for DmManager {
    fn default() -> Self {
        Self {
            peers: Mutex::new(HashMap::new()),
            next_id: Mutex::new(0),
            changed: watch::channel(0).0,
            strangers: Mutex::new(HashMap::new()),
            next_ticket: std::sync::atomic::AtomicU64::new(1),
            gates: Mutex::new(DmGates::default()),
        }
    }
}

/// A stranger slot taken for an incoming connection. Dropped before the connection was
/// kept (a failed hello, a dropped duplicate), it frees the slot it took.
struct StrangerSlot<'a> {
    dm: &'a DmManager,
    peer: UserId,
    /// The ticket of a slot this connection took (None: the peer already had one).
    ticket: Option<u64>,
}

impl StrangerSlot<'_> {
    /// The connection was kept: the slot now lives with it (released by `unregister`, or
    /// when its first message opens a conversation). False if it was evicted meanwhile.
    fn keep(mut self) -> bool {
        let ticket = self.ticket.take();
        ticket.is_none_or(|t| self.dm.strangers.lock().get(&self.peer).is_some_and(|(k, _)| *k == t))
    }
}

impl Drop for StrangerSlot<'_> {
    fn drop(&mut self) {
        if let Some(t) = self.ticket {
            let mut s = self.dm.strangers.lock();
            if s.get(&self.peer).is_some_and(|(k, _)| *k == t) {
                s.remove(&self.peer);
            }
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

/// Keeps the DM connection to a contact up while held, for presence and calls from an open
/// DM view (`Net::dm_watch`). Dropping it lets the connection idle out as usual.
#[derive(Debug)]
pub struct DmWatch {
    net: std::sync::Weak<Inner>,
    peer: UserId,
}

impl Drop for DmWatch {
    fn drop(&mut self) {
        if let Some(net) = self.net.upgrade() {
            let mut peers = net.dm.peers.lock();
            if let Some(p) = peers.get_mut(&self.peer) {
                p.watches = p.watches.saturating_sub(1);
            }
        }
    }
}

/// The frame a watcher sends to keep the contact's side of the connection from idling out.
/// `Tick` is ignored on DM links by every version, so older peers accept it too.
fn keepalive() -> Ctl { Ctl::Tick { epoch: 0, t_ms: now_ms() } }

/// My display name as announced to contacts, if it is a valid name.
fn my_name(net: &Inner) -> Option<String> {
    let name = net.cfg.read().display_name.trim().to_string();
    validate_name(&name).ok().map(|()| name)
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

    /// Is a `DmWatch` on `peer` alive?
    pub fn watched(&self, peer: &UserId) -> bool { self.peers.lock().get(peer).is_some_and(|p| p.watches > 0) }

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

    /// Contacts seen online somewhere (or a new address): deliver pending DMs now, and
    /// reconnect watched contacts.
    pub fn peers_seen(&self, net: &Arc<Inner>, users: &[UserId]) {
        for u in users {
            let known = self.peers.lock().contains_key(u) || net.replica(&ConvId::dm(&net.me, u)).is_some();
            if known && (self.has_pending(net, u) || self.watched(u)) {
                self.ensure_connect(net, *u);
            }
        }
    }

    /// Hold the connection open (calls). The connection is not opened by this call.
    pub fn hold(&self, net: &Arc<Inner>, peer: UserId) -> DmHold {
        self.peers.lock().entry(peer).or_default().holds += 1;
        DmHold { net: Arc::downgrade(net), peer }
    }

    /// Watch `peer`: connect now (or refresh the contact's idle timer when connected), keep
    /// the connection up while the guard lives.
    pub fn watch(&self, net: &Arc<Inner>, peer: UserId) -> DmWatch {
        let guard = DmWatch { net: Arc::downgrade(net), peer };
        if peer == net.me {
            return guard;
        }
        self.peers.lock().entry(peer).or_default().watches += 1;
        // Connected: a keepalive now, so the contact's idle timer restarts before mine checks.
        if !self.send(&peer, &keepalive()) {
            self.ensure_connect(net, peer);
        }
        guard
    }

    /// Dial `peer` unless connected or dialing. A request while a dial is in flight is
    /// remembered: if that dial fails and the address changed meanwhile, the next dial goes
    /// out at once instead of after the backoff.
    pub fn ensure_connect(&self, net: &Arc<Inner>, peer: UserId) {
        if peer == net.me || net.closed.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let hint = net.hint(&peer);
        {
            let mut peers = self.peers.lock();
            let p = peers.entry(peer).or_default();
            if p.link.is_some() {
                return;
            }
            if p.dialing {
                p.redial = true;
                return;
            }
            p.dialing = true;
            p.redial = false;
            p.dial_hint = hint.clone();
            if let Some(r) = p.retry.take() {
                r.abort();
            }
        }
        let net2 = net.clone();
        net.tasks.spawn(async move {
            let res = dial(&net2, peer, hint).await;
            match res {
                Ok(d) => {
                    net2.dm.register(&net2, peer, d, net2.me);
                }
                Err(e) => {
                    tracing::debug!(peer = %peer.short(), error = %e, "DM dial failed");
                    net2.dm.dial_failed(&net2, peer);
                }
            }
        });
    }

    fn dial_failed(&self, net: &Arc<Inner>, peer: UserId) {
        let pending = self.has_pending(net, &peer);
        let hint_now = net.hint(&peer);
        let redial_now = {
            let mut peers = self.peers.lock();
            let p = peers.entry(peer).or_default();
            p.dialing = false;
            // Someone asked again while this dial was out, and the address moved since it
            // started (a contact that restarted on a new port): the old address was the
            // reason it failed, so try the new one now.
            let moved = std::mem::take(&mut p.redial) && p.dial_hint != hint_now;
            p.dial_hint = None;
            if p.link.is_some() {
                // The contact dialed me meanwhile: nothing to retry.
                false
            } else if moved {
                p.failures += 1;
                true
            } else {
                self.schedule_retry(net, peer, p, pending);
                false
            }
        };
        if redial_now {
            tracing::debug!(peer = %peer.short(), "DM address changed during a failed dial; redialing at once");
            self.ensure_connect(net, peer);
        }
        // Waiters (probes) re-check whether a dial is still out.
        self.bump();
    }

    /// Dial again later if the contact is still wanted: undelivered events (the DM backoff)
    /// or a watch (the shorter watch backoff).
    fn schedule_retry(&self, net: &Arc<Inner>, peer: UserId, p: &mut DmPeer, pending: bool) {
        let watched = p.watches > 0;
        if !pending && !watched {
            p.failures = 0;
            return;
        }
        let delay = if watched { net.timings.dm_watch_retry_delay(p.failures) } else { net.timings.dm_retry_delay(p.failures) };
        p.failures += 1;
        let net2 = net.clone();
        let h = net.tasks.spawn(async move {
            tokio::time::sleep(delay).await;
            if let Some(p) = net2.dm.peers.lock().get_mut(&peer) {
                p.retry = None;
            }
            if net2.dm.has_pending(&net2, &peer) || net2.dm.watched(&peer) {
                net2.dm.ensure_connect(&net2, peer);
            }
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

    /// Probe: is `peer` reachable now? See `Net::dm_reachable`.
    pub async fn reachable(&self, net: &Arc<Inner>, peer: UserId) -> bool {
        if self.is_connected(&peer) {
            return true;
        }
        if peer == net.me || net.replica(&ConvId::dm(&net.me, &peer)).is_none() {
            return false;
        }
        {
            let peers = self.peers.lock();
            if let Some(p) = peers.get(&peer)
                && let Some(at) = p.probe_failed_at
                && at.elapsed() < net.timings.dm_watch_retry_delay(p.probe_failures.saturating_sub(1))
            {
                return false;
            }
        }
        let mut rx = self.changed.subscribe();
        self.ensure_connect(net, peer);
        let deadline = Instant::now() + net.timings.dial + net.timings.dial;
        let ok = loop {
            if self.is_connected(&peer) {
                break true;
            }
            if !self.peers.lock().get(&peer).is_some_and(|p| p.dialing) {
                break false;
            }
            match tokio::time::timeout_at(deadline, rx.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => break self.is_connected(&peer),
            }
        };
        let mut peers = self.peers.lock();
        let p = peers.entry(peer).or_default();
        if ok {
            p.probe_failures = 0;
            p.probe_failed_at = None;
        } else {
            p.probe_failures += 1;
            p.probe_failed_at = Some(Instant::now());
        }
        ok
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

    /// My display name changed: announce it on every DM connection.
    pub fn broadcast_profile(&self, net: &Inner) {
        let Some(name) = my_name(net) else { return };
        let Some(b) = link::encode(&Ctl::DmProfile { name }) else { return };
        for p in self.peers.lock().values() {
            if let Some(l) = &p.link {
                let _ = l.live.try_send(b.clone());
            }
        }
    }

    /// Read the persisted announced name of `conv`'s contact into `p` (once).
    fn load_name(net: &Inner, conv: &ConvId, p: &mut DmPeer) {
        if p.name_loaded {
            return;
        }
        p.name_loaded = true;
        if let Ok(Some(v)) = net.storage.get_kv(&name_key(conv))
            && let Ok(s) = String::from_utf8(v)
            && validate_name(&s).is_ok()
        {
            p.name = Some(s);
        }
    }

    /// The display name `peer` announced in `conv`.
    pub fn peer_name(&self, net: &Inner, conv: &ConvId, peer: &UserId) -> Option<String> {
        let mut peers = self.peers.lock();
        let p = peers.entry(*peer).or_default();
        Self::load_name(net, conv, p);
        p.name.clone()
    }

    /// `peer` announced `raw` as its name. Kept if valid, new and within the budget; stored
    /// and announced to the app when the conversation exists (`stored`), otherwise kept in
    /// memory until the stranger's first message opens it.
    fn note_name(&self, net: &Inner, peer: &UserId, conv: &ConvId, raw: &str, stored: bool) {
        let name = raw.trim();
        if validate_name(name).is_err() {
            tracing::debug!(peer = %peer.short(), "ignoring an invalid DM display name");
            return;
        }
        {
            let mut peers = self.peers.lock();
            let p = peers.entry(*peer).or_default();
            Self::load_name(net, conv, p);
            if p.name.as_deref() == Some(name) {
                return;
            }
            if !self.gates.lock().profile.take(peer, Instant::now()) {
                return;
            }
            p.name = Some(name.to_string());
        }
        if stored {
            Self::persist_name(net, peer, conv, name);
        }
    }

    /// A stranger's conversation was just opened: store the name they announced before.
    fn flush_name(&self, net: &Inner, peer: &UserId, conv: &ConvId) {
        let name = self.peers.lock().get(peer).and_then(|p| p.name.clone());
        if let Some(name) = name {
            Self::persist_name(net, peer, conv, &name);
        }
    }

    fn persist_name(net: &Inner, peer: &UserId, conv: &ConvId, name: &str) {
        if let Err(e) = net.storage.set_kv(&name_key(conv), name.as_bytes()) {
            tracing::warn!(error = %e, "persisting a DM display name failed");
        }
        net.emit(NetEvent::DmNameChanged { conv: *conv, peer: *peer, name: name.to_string() });
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

    /// Install a connection whose DmHello exchange is done. False if it was not kept.
    fn register(&self, net: &Arc<Inner>, peer: UserId, d: Dialed, dialer: UserId) -> bool {
        let Ctl::DmHello { conv, heads, .. } = d.reply.clone() else {
            link::close_conn(&d.conn, tincan_core::CLOSE_REJECTED, "expected DmHello");
            if dialer == net.me {
                self.dial_failed(net, peer);
            }
            return false;
        };
        if conv != ConvId::dm(&net.me, &peer) {
            link::close_conn(&d.conn, tincan_core::CLOSE_REJECTED, "wrong conversation");
            if dialer == net.me {
                self.dial_failed(net, peer);
            }
            return false;
        }
        let id = {
            let mut n = self.next_id.lock();
            *n += 1;
            *n
        };
        let (tx, rx) = mpsc::channel(256);
        let sync_tx = tx.clone();
        let new_link = Link::spawn(&net.tasks, id, d.conn.clone(), d.send, d.reader, tx);
        // One lock for "the dial is over" and "the link is up", so a waiter (`reachable`)
        // never sees neither.
        let keep_new = {
            let mut peers = self.peers.lock();
            let p = peers.entry(peer).or_default();
            if dialer == net.me {
                p.dialing = false;
                p.dial_hint = None;
                p.redial = false;
            }
            // Simultaneous dials: keep the connection dialed by the lower user id.
            let keep = p.link.as_ref().is_none_or(|existing| dialer <= existing.dialer);
            if keep {
                if let Some(existing) = &p.link {
                    link::close_conn(&existing.conn, tincan_core::CLOSE_NORMAL, "duplicate");
                }
                p.link = Some(DmLink { id, dialer, conn: d.conn.clone(), live: new_link.live() });
                p.failures = 0;
                p.probe_failures = 0;
                p.probe_failed_at = None;
                if let Some(r) = p.retry.take() {
                    r.abort();
                }
            }
            keep
        };
        if !keep_new {
            new_link.close(tincan_core::CLOSE_NORMAL, "duplicate");
            // Waiters re-check: the dial is over and the kept connection is up.
            self.bump();
            return false;
        }
        if let Some(name) = my_name(net) {
            let _ = new_link.send(&Ctl::DmProfile { name });
        }
        self.bump();
        net.media.dm_link(net, peer);
        net.emit(NetEvent::DmLinkUp { peer });
        tracing::debug!(peer = %peer.short(), "DM connection up");
        let net2 = net.clone();
        net.tasks.spawn(run_conn(net2, peer, conv, new_link, rx, sync_tx, heads));
        true
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
            // Undelivered events or a watch: dial again on the backoff (a dial already out
            // reschedules itself when it fails).
            let pending = self.has_pending(net, &peer);
            {
                let mut peers = self.peers.lock();
                if let Some(p) = peers.get_mut(&peer)
                    && p.link.is_none()
                    && !p.dialing
                {
                    self.schedule_retry(net, peer, p, pending);
                }
            }
            // A stranger that left nothing behind is forgotten.
            let conv = ConvId::dm(&net.me, &peer);
            if net.replica(&conv).is_none() {
                let mut peers = self.peers.lock();
                if peers.get(&peer).is_some_and(|p| p.link.is_none() && p.holds == 0 && p.watches == 0 && p.retry.is_none() && !p.dialing) {
                    peers.remove(&peer);
                }
            }
        }
    }

    /// May a stranger's DM connection be accepted now? A global rate, and at most
    /// `MAX_STRANGER_LINKS` stranger connections at once: when full, the oldest one is closed
    /// to make room (identities cost nothing, so refusing newcomers would let idle holders
    /// lock everyone else out). The slot is freed when the returned guard is dropped unless
    /// the connection was kept (`StrangerSlot::keep`).
    fn admit_stranger(&self, peer: UserId) -> Option<StrangerSlot<'_>> {
        let (evicted, ticket) = {
            let mut s = self.strangers.lock();
            if s.contains_key(&peer) {
                return Some(StrangerSlot { dm: self, peer, ticket: None });
            }
            if !self.gates.lock().strangers.take(Instant::now()) {
                return None;
            }
            let evicted = if s.len() >= MAX_STRANGER_LINKS {
                let oldest = s.iter().min_by_key(|(_, (_, at))| *at).map(|(u, _)| *u);
                oldest.inspect(|u| {
                    s.remove(u);
                })
            } else {
                None
            };
            let ticket = self.next_ticket.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            s.insert(peer, (ticket, Instant::now()));
            (evicted, ticket)
        };
        if let Some(old) = evicted {
            tracing::debug!(stranger = %old.short(), "stranger DM connections full; closing the oldest");
            let conn = self.peers.lock().get(&old).and_then(|p| p.link.as_ref().map(|l| l.conn.clone()));
            if let Some(c) = conn {
                link::close_conn(&c, tincan_core::CLOSE_NORMAL, "busy");
            }
        }
        Some(StrangerSlot { dm: self, peer, ticket: Some(ticket) })
    }

    /// Stranger slots taken now.
    #[cfg(test)]
    pub(crate) fn stranger_slots(&self) -> usize { self.strangers.lock().len() }

    /// Room for a new event of `bytes` from `peer` in the DM budget (`strict`: a stranger's
    /// conversation I never wrote in). Taken if there is.
    fn allow_event(&self, peer: &UserId, strict: bool, bytes: usize) -> bool {
        let now = Instant::now();
        let mut g = self.gates.lock();
        let count = if strict { g.unanswered.take(peer, now) } else { g.events.take(peer, now) };
        count && g.event_bytes.take_n(peer, bytes as f64, now)
    }

    /// Has the DM budget of `peer` room again (after events were cut)?
    fn events_ready(&self, peer: &UserId, strict: bool) -> bool {
        let now = Instant::now();
        let mut g = self.gates.lock();
        if strict { g.unanswered.ready(peer, now) } else { g.events.ready(peer, now) }
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

/// Does a DM conversation count toward `MAX_UNANSWERED_STRANGER_CONVS`? One the stranger
/// opened (they wrote, I never did, we share no server) within `STALE_STRANGER_CONV_MS`. A
/// conversation I opened myself, or an old one, does not: old spam never locks out new
/// people.
fn counts_as_unanswered(my_head: u64, their_head: u64, shares_server: bool, opened_ms: u64, now_ms: u64) -> bool {
    my_head == 0 && their_head > 0 && !shares_server && now_ms.saturating_sub(opened_ms) < STALE_STRANGER_CONV_MS
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
    let now = now_ms();
    dms.iter()
        .filter(|(p, r)| {
            let (mine, theirs, opened) = {
                let g = r.lock();
                (g.state.head(&net.me), g.state.head(p), g.record.joined_ms)
            };
            counts_as_unanswered(mine, theirs, net.shares_server(p), opened, now)
        })
        .count()
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

async fn dial(net: &Arc<Inner>, peer: UserId, hint: Option<AddrHint>) -> std::result::Result<Dialed, link::DialError> {
    let (conv, _) = open_conv(net, peer).map_err(|e| link::DialError::Unreachable(e.to_string()))?;
    let conn = link::connect(&net.endpoint, &peer, hint.as_ref(), ALPN_DM, net.timings.dial).await?;
    let hello = dm_hello(net, &conv);
    link::exchange(conn, &hello, net.timings.dial).await
}

fn dm_hello(net: &Inner, conv: &ConvId) -> Ctl {
    let heads = net.replica(conv).map(|r| r.lock().state.sync_heads()).unwrap_or_default();
    // No Have list: `who_has` in a DM always names the contact, and my held blobs of other
    // conversations are none of their business.
    Ctl::DmHello { conv: *conv, heads, have: Vec::new() }
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
    let slot = if net.is_known_peer(&peer) {
        if let Err(e) = open_conv(&net, peer) {
            tracing::warn!(error = %e, "cannot open DM conversation");
            link::close_conn(&conn, tincan_core::CLOSE_REJECTED, "internal");
            return;
        }
        None
    } else {
        // A stranger (no conversation, no shared server): accepted only within the limits,
        // and nothing is stored until they send a valid first message.
        match net.dm.admit_stranger(peer) {
            Some(s) => Some(s),
            None => {
                link::close_conn(&conn, tincan_core::CLOSE_REJECTED, "busy");
                return;
            }
        }
    };
    // A peer that stops the stream or grants it no credit cannot park this task (or keep
    // its stranger slot: the guard frees it on every early return).
    let hello = dm_hello(&net, conv);
    match tokio::time::timeout(net.timings.dial, link::write_ctl(&mut send, &hello)).await {
        Ok(Ok(())) => {}
        _ => {
            link::close_conn(&conn, tincan_core::CLOSE_NORMAL, "no hello");
            return;
        }
    }
    let d = Dialed { conn: conn.clone(), send, reader, reply: first };
    let kept = net.dm.register(&net, peer, d, peer);
    // Not kept (a duplicate): the slot is freed as it drops here.
    if let Some(slot) = slot
        && kept
        && !slot.keep()
    {
        // Evicted while the hello went out: this connection is the one to go.
        link::close_conn(&conn, tincan_core::CLOSE_NORMAL, "busy");
    }
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

/// The empty last page of a `send_missing` run (live events never come as an empty page).
fn is_sync_end(ctl: &Ctl) -> bool { matches!(ctl, Ctl::Events { events, more: false } if events.is_empty()) }

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
    // The contact's initial sync: done at once when their hello shows nothing I lack,
    // otherwise with the empty last page of their `send_missing` (sent whenever I lack
    // something, by every version).
    let mut sync_pending = false;
    // A stranger whose first message opens the conversation below: their history follows
    // if they had any when we connected.
    let stranger_history = first_heads.0.iter().any(|(_, s)| *s > 0);
    if let Some(r) = &replica {
        let mine = r.lock().state.sync_heads();
        if first_heads.missing_in(&mine).is_empty() {
            net.emit(NetEvent::SyncComplete { conv });
        } else {
            sync_pending = true;
        }
        on_heads(&net, &peer, &conv, r, &link, &mut sync, first_heads);
    }
    let idle = net.timings.dm_idle;
    let keepalive_every = net.timings.dm_keepalive;
    let mut idle_at = Instant::now() + idle;
    let mut ping_at = Instant::now() + keepalive_every;
    // Events were cut by the DM budget: ask for the rest at this time (once it has room).
    let mut refetch_at: Option<Instant> = None;
    loop {
        let wake = refetch_at.map_or(idle_at.min(ping_at), |r| r.min(idle_at).min(ping_at));
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(LinkEvent::Frame { ctl, .. }) => {
                    idle_at = Instant::now() + idle;
                    let ctl = *ctl;
                    let mut opened_live = false;
                    if replica.is_none() {
                        if let Ctl::DmProfile { name } = &ctl {
                            net.dm.note_name(&net, &peer, &conv, name, false);
                            continue;
                        }
                        replica = stranger_frame(&net, &peer, &conv, &ctl);
                        if replica.is_none() {
                            if is_extra(&ctl) && net.dm.allow(&peer, &ctl) {
                                net.extra(CtlSource::Dm { peer }, ctl);
                            }
                            continue;
                        }
                        sync_pending = stranger_history;
                        opened_live = !stranger_history;
                    }
                    if let Some(r) = &replica {
                        let end = sync_pending && is_sync_end(&ctl);
                        if on_frame(&net, &peer, &conv, r, &link, &mut sync, ctl) && refetch_at.is_none() {
                            refetch_at = Some(Instant::now() + DM_REFETCH_EVERY);
                        }
                        if end || opened_live {
                            sync_pending = false;
                            net.emit(NetEvent::SyncComplete { conv });
                        }
                    }
                }
                Some(LinkEvent::SyncSent { .. }) => {
                    if let Some(r) = &replica {
                        sync.finished(&net, r, &link);
                    }
                }
                Some(LinkEvent::Closed { .. }) | None => break,
            },
            _ = tokio::time::sleep_until(wake) => {
                let now = Instant::now();
                if let Some(r) = &replica
                    && refetch_at.is_some_and(|t| now >= t)
                {
                    if net.dm.events_ready(&peer, strict_budget(&net, &peer, r)) {
                        // The sender streamed its missing events once; what was cut comes
                        // when it hears my heads again.
                        refetch_at = None;
                        let heads = r.lock().state.sync_heads();
                        let _ = link.send(&Ctl::SyncHave { heads });
                    } else {
                        refetch_at = Some(now + DM_REFETCH_EVERY);
                    }
                }
                let (held, watched) = net.dm.peers.lock().get(&peer).map(|p| (p.holds > 0, p.watches > 0)).unwrap_or_default();
                if now >= ping_at {
                    ping_at = now + keepalive_every;
                    if watched {
                        let _ = link.send(&keepalive());
                    }
                }
                if now >= idle_at {
                    if held || watched || net.dm.has_pending(&net, &peer) {
                        idle_at = now + idle;
                    } else {
                        tracing::debug!(peer = %peer.short(), "closing idle DM connection");
                        link.close(tincan_core::CLOSE_NORMAL, "idle");
                        break;
                    }
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
            net.dm.flush_name(net, peer, conv);
            net.replica(conv)
        }
        Err(e) => {
            tracing::warn!(error = %e, "cannot open DM conversation");
            None
        }
    }
}

/// Is the DM with `peer` held to the stranger budget (`DM_UNANSWERED_BURST`)? While I never
/// wrote in it and we share no server.
fn strict_budget(net: &Inner, peer: &UserId, replica: &ReplicaRef) -> bool { replica.lock().state.head(&net.me) == 0 && !net.shares_server(peer) }

/// Handle one frame of an open conversation. True if events in it were cut by the DM
/// budget (ask for the rest again once it has room).
fn on_frame(net: &Arc<Inner>, peer: &UserId, conv: &ConvId, replica: &ReplicaRef, link: &Link, sync: &mut DmSync, ctl: Ctl) -> bool {
    match ctl {
        Ctl::Events { events, .. } => {
            if events.is_empty() {
                return false;
            }
            let strict = strict_budget(net, peer, replica);
            let mut applied = Vec::new();
            let mut cut = false;
            {
                let mut r = replica.lock();
                let mut verified = Vec::with_capacity(events.len());
                for signed in events {
                    let Ok(s) = StoredEvent::from_signed(signed) else { continue };
                    if s.conv() != *conv {
                        continue;
                    }
                    // New events are budgeted per contact; over it the rest of the page is
                    // not applied (and not acknowledged).
                    if s.event.seq > r.state.head(&s.event.author) && !net.dm.allow_event(peer, strict, s.signed.bytes.len()) {
                        cut = true;
                        break;
                    }
                    verified.push(s);
                }
                for res in r.ingest_many(net.storage.as_ref(), &net.hlc, verified) {
                    applied.extend(res.applied);
                }
            }
            if !applied.is_empty() {
                net.emit_applied(*conv, &applied);
                let heads = replica.lock().state.sync_heads();
                let _ = link.send(&Ctl::SyncHave { heads });
            }
            return cut;
        }
        Ctl::SyncHave { heads } => on_heads(net, peer, conv, replica, link, sync, heads),
        Ctl::Have { .. } => {}
        Ctl::Typing { by, .. } => {
            if by == *peer && net.dm.allow(peer, &ctl) {
                net.emit(NetEvent::Typing { conv: *conv, channel: dm_channel(conv), user: by });
            }
        }
        Ctl::DmHello { heads, .. } => on_heads(net, peer, conv, replica, link, sync, heads),
        Ctl::DmProfile { name } => net.dm.note_name(net, peer, conv, &name, true),
        // A watching contact's keepalive; receiving it already restarted the idle timer.
        Ctl::Tick { .. } => {}
        other if is_extra(&other) => {
            if net.dm.allow(peer, &other) {
                net.extra(CtlSource::Dm { peer: *peer }, other);
            }
        }
        other => tracing::trace!(?other, "DM: ignoring frame"),
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NET-SECURITY-6: only fresh conversations a stranger opened count toward the cap, so
    /// old spam (or a DM I opened myself and never wrote in) never locks out new people.
    #[test]
    fn only_fresh_stranger_opened_conversations_count() {
        let now = 10 * STALE_STRANGER_CONV_MS;
        assert!(counts_as_unanswered(0, 3, false, now - 1000, now), "a stranger wrote, I did not");
        assert!(!counts_as_unanswered(1, 3, false, now - 1000, now), "answered");
        assert!(!counts_as_unanswered(0, 0, false, now - 1000, now), "I opened it and nobody wrote");
        assert!(!counts_as_unanswered(0, 3, true, now - 1000, now), "we share a server");
        assert!(!counts_as_unanswered(0, 3, false, now - STALE_STRANGER_CONV_MS, now), "older than the window");
    }
}
