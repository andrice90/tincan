//! One conversation's replica: derived state, pending buffer, persistence and history sync.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;
use tincan_core::event::{Body, StoredEvent};
use tincan_core::hlc::Hlc;
use tincan_core::keys::ServerKeys;
use tincan_core::state::{Change, ConvState, PendingBuffer, PendingLimits, Verdict};
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
    /// Servers: when this replica first stored each member's admission and who admitted
    /// them (persisted). Local knowledge, kept out of `ConvState` (which must come out the
    /// same for any delivery order): admission budgets and merge attestations use it.
    seen: HashMap<UserId, Seen>,
}

/// When a replica first stored a member's admission, by its own clock, and who admitted
/// them: the `MemberAdmit`'s author, or for a join I admitted as host, the creator of the
/// invite used. None for the owner and for members stored before this was recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Seen {
    /// Unix ms (local clock); 0 = before records were kept (counts as long ago).
    pub at_ms: u64,
    /// Who admitted them.
    pub by: Option<UserId>,
}

/// Longest admitter chain walked (a deeper chain stops at this many steps).
const MAX_CHAIN: usize = 64;

fn seen_key(conv: &ConvId) -> String { format!("net/members_seen/{}", conv.to_hex()) }

fn load_seen(storage: &dyn Storage, conv: &ConvId) -> Option<HashMap<UserId, Seen>> {
    let raw = match storage.get_kv(&seen_key(conv)) {
        Ok(Some(v)) => v,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(error = %e, "reading member records failed");
            return None;
        }
    };
    match postcard::from_bytes::<Vec<(UserId, u64, Option<UserId>)>>(&raw) {
        Ok(v) => Some(v.into_iter().map(|(u, at_ms, by)| (u, Seen { at_ms, by })).collect()),
        Err(e) => {
            tracing::warn!(error = %e, "member records unreadable; starting over");
            None
        }
    }
}

/// Result of [`Replica::ingest`].
#[derive(Debug)]
pub(crate) struct Ingested {
    /// Verdict for the incoming event.
    pub verdict: Verdict,
    /// Every event applied and stored, in order (the incoming one first if accepted).
    pub applied: Vec<(StoredEvent, Change)>,
    /// The incoming event and the stored one are two different events by the same author
    /// with the same seq (equivocation). Both are signed: a proof.
    pub conflict: Option<StoredEvent>,
}

/// A stored own-seq floor further than this above the applied head can only come from a
/// host that fed us a bogus `your_next_seq`; events signed that far ahead never apply, so
/// reusing the lower seqs is safe.
const FLOOR_POISON_GAP: u64 = 1_000_000;

fn floor_key(conv: &ConvId) -> String { format!("net/own_seq/{}", conv.to_hex()) }

fn equiv_key(conv: &ConvId, author: &UserId) -> String { format!("net/equiv/{}/{}", conv.to_hex(), author.to_hex()) }

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
        let limits = if record.is_dm { PendingLimits::dm() } else { PendingLimits::network() };
        let mut seen = HashMap::new();
        let mut save = false;
        if !record.is_dm {
            match load_seen(storage, &record.id) {
                Some(m) => seen = m,
                None => {
                    // First load since records are kept: everyone known now counts as stored
                    // long ago. A new conversation starts with an empty record.
                    for u in state.members.keys() {
                        seen.insert(*u, Seen { at_ms: 0, by: None });
                    }
                    save = true;
                }
            }
        }
        let r = Self { record, keys, state, pending: PendingBuffer::with_limits(limits), own_floor, seen };
        if save {
            r.save_seen(storage);
        }
        r
    }

    fn save_seen(&self, storage: &dyn Storage) {
        let v: Vec<(UserId, u64, Option<UserId>)> = self.seen.iter().map(|(u, s)| (*u, s.at_ms, s.by)).collect();
        match postcard::to_stdvec(&v) {
            Ok(bytes) => {
                if let Err(e) = storage.set_kv(&seen_key(&self.record.id), &bytes) {
                    tracing::error!(error = %e, "persisting member records failed");
                }
            }
            Err(e) => tracing::error!(error = %e, "encoding member records failed"),
        }
    }

    /// Record first sightings of members admitted by `applied` (servers only).
    fn note_admissions<'a>(&mut self, storage: &dyn Storage, applied: impl Iterator<Item = &'a StoredEvent>) {
        if self.record.is_dm {
            return;
        }
        let now = now_ms();
        let mut changed = false;
        for e in applied {
            let (user, by) = match &e.event.body {
                Body::Genesis { .. } => (e.event.author, None),
                Body::MemberAdmit { user, .. } => (*user, Some(e.event.author)),
                _ => continue,
            };
            if let std::collections::hash_map::Entry::Vacant(v) = self.seen.entry(user) {
                v.insert(Seen { at_ms: now, by: by.filter(|b| *b != user) });
                changed = true;
            }
        }
        if changed {
            self.save_seen(storage);
        }
    }

    /// I admitted `joiner` as host for a join through an invite of `creator`: that creator is
    /// who admitted them, not me. Only for a record made by my admission (a rejoin keeps its
    /// own).
    pub fn note_join(&mut self, storage: &dyn Storage, me: &UserId, joiner: &UserId, creator: &UserId) {
        if let Some(s) = self.seen.get_mut(joiner)
            && s.by == Some(*me)
            && creator != joiner
        {
            s.by = Some(*creator);
            self.save_seen(storage);
        }
    }

    /// Is `user` a member first stored at or before `cutoff_ms` (local clock)? Members with
    /// no record count as stored long ago.
    pub fn seen_by(&self, user: &UserId, cutoff_ms: u64) -> bool { self.seen.get(user).is_none_or(|s| s.at_ms <= cutoff_ms) }

    /// Who answers for admissions authored by `user`: the nearest admitter up the chain
    /// (`user` first) that this replica stored at or before `cutoff_ms`. A member admitted
    /// since then spends its admitter's budget, never a fresh one of its own. A chain with
    /// nobody that old ends at its top.
    pub fn answerable(&self, user: &UserId, cutoff_ms: u64) -> UserId {
        let mut cur = *user;
        for _ in 0..MAX_CHAIN {
            match self.seen.get(&cur) {
                Some(s) if s.at_ms > cutoff_ms => match s.by {
                    Some(by) if by != cur => cur = by,
                    _ => return cur,
                },
                _ => return cur,
            }
        }
        cur
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
        let mut r = Self::new(storage, record, state);
        r.repair_floor(storage, me);
        Ok(r)
    }

    /// A floor poisoned by a host's bogus seq hint (far above the head) is reset to the head.
    fn repair_floor(&mut self, storage: &dyn Storage, me: &UserId) {
        let head = self.state.head(me);
        if self.own_floor > self.state.next_seq(me).saturating_add(FLOOR_POISON_GAP) {
            tracing::warn!(conv = %self.record.id.short(), floor = self.own_floor, head, "own seq floor far above the head: reset");
            self.own_floor = head;
            if let Err(e) = storage.set_kv(&floor_key(&self.record.id), &head.to_le_bytes()) {
                tracing::error!(error = %e, "persisting own seq floor failed");
            }
        }
    }

    /// Validate, apply and persist one event (buffering it if early), plus everything it
    /// unblocked. Storage failures are logged; the state still advances. A second event for
    /// an (author, seq) already stored is reported in `conflict` (and its proof kept in kv).
    pub fn ingest(&mut self, storage: &dyn Storage, hlc: &Hlc, ev: StoredEvent) -> Ingested {
        // Only an event at or below its author's head can be a second version of a stored one.
        let incoming = (ev.event.seq <= self.state.head(&ev.event.author)).then(|| ev.clone());
        let (verdict, applied) = self.pending.ingest(&mut self.state, ev, now_ms());
        let conflict = match (&verdict, &incoming) {
            (Verdict::Stale, Some(inc)) => self.find_conflict(storage, inc),
            _ => None,
        };
        for (e, _) in &applied {
            hlc.observe(e.event.hlc);
        }
        self.persist(storage, applied.iter());
        Ingested { verdict, applied, conflict }
    }

    /// `ingest` for a batch (a sync page): same verdicts, applied events and conflicts, in
    /// order, but everything applied is stored with one `Storage::insert_events` call (one
    /// SQLite transaction) instead of one insert per event. Before a stale event is checked
    /// for equivocation, what was applied so far is stored, so the check sees it.
    pub fn ingest_many(&mut self, storage: &dyn Storage, hlc: &Hlc, evs: Vec<StoredEvent>) -> Vec<Ingested> {
        let mut out: Vec<Ingested> = Vec::with_capacity(evs.len());
        // First entry of `out` whose applied events are not stored yet.
        let mut unstored = 0;
        for ev in evs {
            let incoming = (ev.event.seq <= self.state.head(&ev.event.author)).then(|| ev.clone());
            let (verdict, applied) = self.pending.ingest(&mut self.state, ev, now_ms());
            for (e, _) in &applied {
                hlc.observe(e.event.hlc);
            }
            let conflict = match (&verdict, &incoming) {
                (Verdict::Stale, Some(inc)) => {
                    self.persist(storage, out[unstored..].iter().flat_map(|i| i.applied.iter()));
                    unstored = out.len();
                    self.find_conflict(storage, inc)
                }
                _ => None,
            };
            out.push(Ingested { verdict, applied, conflict });
        }
        self.persist(storage, out[unstored..].iter().flat_map(|i| i.applied.iter()));
        out
    }

    /// Store applied events (one `insert_events` call), then flag deleted targets. Storage
    /// failures are logged; the state has already advanced.
    fn persist<'a>(&mut self, storage: &dyn Storage, applied: impl Iterator<Item = &'a (StoredEvent, Change)>) {
        let applied: Vec<&(StoredEvent, Change)> = applied.collect();
        if applied.is_empty() {
            return;
        }
        self.note_admissions(storage, applied.iter().map(|(e, _)| e));
        let evs: Vec<&StoredEvent> = applied.iter().map(|(e, _)| e).collect();
        for (res, e) in storage.insert_events(&evs).into_iter().zip(&evs) {
            if let Err(err) = res {
                tracing::error!(error = %err, id = %e.id.short(), "storing event failed");
            }
        }
        for (_, change) in &applied {
            if let Change::Deleted { target } = change
                && let Err(err) = storage.mark_deleted(&self.state.conv, target)
            {
                tracing::error!(error = %err, "marking event deleted failed");
            }
        }
    }

    /// `ev` was Stale: is a different event stored for its (author, seq)? Then keep both as
    /// a proof (first proof per author only) and return the stored one.
    fn find_conflict(&self, storage: &dyn Storage, ev: &StoredEvent) -> Option<StoredEvent> {
        if matches!(storage.event(&ev.id), Ok(Some(_))) {
            return None;
        }
        let (author, seq) = (ev.event.author, ev.event.seq);
        let stored = storage.events_range(&self.state.conv, &author, seq, seq).ok()?.into_iter().find(|e| e.id != ev.id)?;
        let key = equiv_key(&self.state.conv, &author);
        if matches!(storage.get_kv(&key), Ok(None))
            && let Ok(bytes) = postcard::to_stdvec(&(&stored.signed, &ev.signed))
            && let Err(e) = storage.set_kv(&key, &bytes)
        {
            tracing::warn!(error = %e, "persisting an equivocation proof failed");
        }
        tracing::warn!(conv = %self.state.conv.short(), author = %author.short(), seq, "author signed two different events with the same seq");
        Some(stored)
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
