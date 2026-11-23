//! One conversation's replica: derived state, pending buffer, persistence and history sync.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;
use tincan_core::event::StoredEvent;
use tincan_core::hlc::Hlc;
use tincan_core::keys::ServerKeys;
use tincan_core::state::{Change, ConvState, PendingBuffer, Verdict};
use tincan_core::storage::{ConvRecord, Storage};
use tincan_core::sync::{Heads, SYNC_PAGE_BYTES};
use tincan_core::wire::Ctl;
use tincan_core::{ConvId, UserId, now_ms};
use tokio::sync::mpsc;

use crate::error::Result;
use crate::link::encode;

/// Seqs read from storage per call while syncing.
const SYNC_READ_BATCH: u64 = 256;
/// Per-event overhead estimate inside an `Events` frame.
const EVENT_FRAME_OVERHEAD: usize = 80;

/// Shared handle to a replica.
pub(crate) type ReplicaRef = Arc<Mutex<Replica>>;

/// Derived state plus bookkeeping of one server or DM.
#[derive(Debug)]
pub(crate) struct Replica {
    /// The stored conversation record.
    pub record: ConvRecord,
    /// Server keys (servers only).
    pub keys: Option<ServerKeys>,
    /// Validated state.
    pub state: ConvState,
    /// Early events waiting for prerequisites.
    pub pending: PendingBuffer,
    /// Highest own seq ever signed for this conversation (persisted), so a seq is never
    /// signed twice even if the signed event was not stored locally.
    own_floor: u64,
}

/// Result of [`Replica::ingest`].
#[derive(Debug)]
pub(crate) struct Ingested {
    /// Verdict for the incoming event.
    pub verdict: Verdict,
    /// Every event applied and stored, in order (the incoming one first if accepted).
    pub applied: Vec<(StoredEvent, Change)>,
}

fn floor_key(conv: &ConvId) -> String { format!("net/own_seq/{}", conv.to_hex()) }

fn load_floor(storage: &dyn Storage, conv: &ConvId) -> u64 {
    match storage.get_kv(&floor_key(conv)) {
        Ok(Some(v)) if v.len() == 8 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&v);
            u64::from_le_bytes(b)
        }
        Ok(_) => 0,
        Err(e) => {
            tracing::warn!(error = %e, "reading own seq floor failed");
            0
        }
    }
}

impl Replica {
    /// A replica over an existing (possibly empty) state.
    pub fn new(storage: &dyn Storage, record: ConvRecord, state: ConvState) -> Self {
        let keys = record.server_secret.map(|s| ServerKeys::derive(&s, &record.id));
        let own_floor = load_floor(storage, &record.id);
        Self { record, keys, state, pending: PendingBuffer::new(), own_floor }
    }

    /// Rebuild a replica from every stored event of `record`'s conversation.
    pub fn load(storage: &dyn Storage, record: ConvRecord, me: &UserId) -> Result<Self> {
        let conv = record.id;
        let mut events = Vec::new();
        for (author, head) in storage.heads(&conv)? {
            events.extend(storage.events_range(&conv, &author, 1, head)?);
        }
        let parties = if record.is_dm { record.peer.map(|p| (*me, p)) } else { None };
        let state = ConvState::rebuild(conv, parties, events, now_ms());
        Ok(Self::new(storage, record, state))
    }

    /// Validate, apply and persist one event (buffering it if early), plus everything it
    /// unblocked. Storage failures are logged; the state still advances.
    pub fn ingest(&mut self, storage: &dyn Storage, hlc: &Hlc, ev: StoredEvent) -> Ingested {
        let (verdict, applied) = self.pending.ingest(&mut self.state, ev, now_ms());
        for (e, change) in &applied {
            hlc.observe(e.event.hlc);
            if let Err(err) = storage.insert_event(e) {
                tracing::error!(error = %err, id = %e.id.short(), "storing event failed");
            }
            if let Change::Deleted { target } = change
                && let Err(err) = storage.mark_deleted(&self.state.conv, target)
            {
                tracing::error!(error = %err, "marking event deleted failed");
            }
        }
        Ingested { verdict, applied }
    }

    /// The seq my next event must carry: past both the applied head and every seq I signed.
    pub fn next_own_seq(&self, me: &UserId) -> u64 { self.state.next_seq(me).max(self.own_floor.saturating_add(1)) }

    /// Record that `seq` was signed (persisted before the event leaves this device).
    pub fn reserve_own_seq(&mut self, storage: &dyn Storage, seq: u64) {
        if seq > self.own_floor {
            self.own_floor = seq;
            if let Err(e) = storage.set_kv(&floor_key(&self.record.id), &seq.to_le_bytes()) {
                tracing::error!(error = %e, "persisting own seq floor failed");
            }
        }
    }

    /// The host discarded my event `seq`: the seq may be reused.
    /// The floor is kept across `leave` (not deleted with the conversation), so a re-join
    /// continues the author's seq chain after its `MemberLeave`.
    pub fn release_own_seq(&mut self, storage: &dyn Storage, seq: u64) {
        if self.own_floor == seq {
            self.own_floor = seq.saturating_sub(1);
            if let Err(e) = storage.set_kv(&floor_key(&self.record.id), &self.own_floor.to_le_bytes()) {
                tracing::error!(error = %e, "persisting own seq floor failed");
            }
        }
    }
}

/// Persist an own-seq floor for a conversation that has no replica yet (join flow).
pub(crate) fn store_floor(storage: &dyn Storage, conv: &ConvId, seq: u64) {
    if load_floor(storage, conv) < seq
        && let Err(e) = storage.set_kv(&floor_key(conv), &seq.to_le_bytes())
    {
        tracing::error!(error = %e, "persisting own seq floor failed");
    }
}

/// Own-seq floor of a conversation (0 if none), for the join flow.
pub(crate) fn stored_floor(storage: &dyn Storage, conv: &ConvId) -> u64 { load_floor(storage, conv) }

/// Stream every event `theirs` lacks over `bulk`, in pages of at most `SYNC_PAGE_BYTES`,
/// authors in `ConvState::sync_author_order`, each ascending by seq. The last page has
/// `more: false` (an empty page if nothing was missing). Returns the number of events sent.
pub(crate) async fn send_missing(storage: Arc<dyn Storage>, replica: ReplicaRef, theirs: Heads, bulk: mpsc::Sender<Bytes>) -> usize {
    let (conv, plan) = {
        let r = replica.lock();
        let mine = r.state.sync_heads();
        let mut ranges: HashMap<UserId, (u64, u64)> = mine.missing_in(&theirs).into_iter().map(|(u, a, b)| (u, (a, b))).collect();
        let mut plan = Vec::with_capacity(ranges.len());
        for author in r.state.sync_author_order() {
            if let Some(range) = ranges.remove(&author) {
                plan.push((author, range));
            }
        }
        // Authors without a known order (should not happen) go last.
        let mut rest: Vec<(UserId, (u64, u64))> = ranges.into_iter().collect();
        rest.sort();
        plan.extend(rest);
        (r.state.conv, plan)
    };
    let mut sent = 0usize;
    let mut page = Vec::new();
    let mut page_bytes = 0usize;
    for (author, (from, to)) in plan {
        let mut s = from;
        while s <= to {
            let e = to.min(s.saturating_add(SYNC_READ_BATCH - 1));
            let batch = match storage.events_range(&conv, &author, s, e) {
                Ok(b) => b,
                Err(err) => {
                    tracing::warn!(error = %err, "sync: reading events failed");
                    Vec::new()
                }
            };
            for ev in batch {
                page_bytes += ev.signed.bytes.len() + EVENT_FRAME_OVERHEAD;
                page.push(ev.signed);
                if page_bytes >= SYNC_PAGE_BYTES {
                    sent += page.len();
                    let msg = Ctl::Events { events: std::mem::take(&mut page), more: true };
                    page_bytes = 0;
                    if !send_bulk(&bulk, &msg).await {
                        return sent;
                    }
                }
            }
            if e == u64::MAX {
                break;
            }
            s = e + 1;
        }
    }
    if !page.is_empty() {
        sent += page.len();
        if !send_bulk(&bulk, &Ctl::Events { events: page, more: true }).await {
            return sent;
        }
    }
    // An empty final page marks the end of this sync unambiguously (live fan-out never
    // sends an empty `Events`).
    let _ = send_bulk(&bulk, &Ctl::Events { events: Vec::new(), more: false }).await;
    sent
}

async fn send_bulk(bulk: &mpsc::Sender<Bytes>, msg: &Ctl) -> bool {
    match encode(msg) {
        Some(b) => bulk.send(b).await.is_ok(),
        None => true,
    }
}
