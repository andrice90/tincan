//! Persistence interface used by tincan-net. `tincan-store::SqliteStorage` is the
//! real implementation; `MemStorage` is the reference implementation for tests.
//!
//! Semantics every impl must follow:
//! - Events are indexed by `StoredEvent::conv()`, NOT `event.server`: a Genesis event carries
//!   `server = [0; 32]` and belongs to the conversation named by its own id.
//! - `insert_event` is idempotent by id (returns false for a duplicate) and does not
//!   validate (the caller already ran ConvState). It stores the `deleted` flag as given.
//!   `insert_events` does the same for a slice, with one result per event.
//! - `heads(conv)` = highest seq per author among stored events of that conv, sorted by author.
//! - `events_range(conv, author, from, to)` inclusive, ascending seq.
//! - `messages(conv, channel, before, limit)` = Message events of that channel (all channels
//!   when `None`), deleted ones included with `deleted = true` (hiding their text is the
//!   caller's concern), ordered by (hlc, id) DESCENDING, strictly before `before` when given.
//! - `state_events(conv)` = every non-Message event (Delete events included), ascending by
//!   (author, seq). Useful for sync ordering. NOT enough for `ConvState::rebuild`, which needs
//!   every event (heads and Delete authority depend on messages): rebuild from
//!   `heads` + `events_range(conv, author, 1, head)` per author.
//! - `mark_deleted` sets the flag on the target if stored (remembering it for a target that
//!   arrives later is NOT required). It must keep the signed bytes: deleted events are still
//!   synced to other members, because seqs are contiguous.
//! - `delete_conv` removes the conversation record, its events and its peers.
//! - `convs()` is ordered by (joined_ms, id); `peers` most recently seen first.

use std::collections::{BTreeSet, HashMap};

use crate::event::{Body, StoredEvent};
use crate::wire::AddrHint;
use crate::{ChannelId, ConvId, CoreError, EventId, Result, UserId};

/// A server (or DM) the local user is part of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConvRecord {
    /// Server id or DM conversation id.
    pub id: ConvId,
    /// A DM.
    pub is_dm: bool,
    /// For DMs: the other user.
    pub peer: Option<UserId>,
    /// Cached display name (server name or contact name).
    pub name: String,
    /// Servers only.
    pub server_secret: Option<[u8; 32]>,
    /// Highest host epoch seen.
    pub last_epoch: u64,
    /// Last known host.
    pub last_host: Option<UserId>,
    /// When the local user joined (unix ms).
    pub joined_ms: u64,
}

/// A cached address for a member of a conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerRecord {
    /// The member.
    pub user: UserId,
    /// Last known address.
    pub addr: AddrHint,
    /// When they were last seen online (unix ms).
    pub last_seen_ms: u64,
}

/// Persistence used by tincan-net (see module docs for the required semantics).
pub trait Storage: Send + Sync + 'static {
    /// Insert or replace a conversation record.
    fn upsert_conv(&self, c: &ConvRecord) -> Result<()>;
    /// One conversation record.
    fn conv(&self, id: &ConvId) -> Result<Option<ConvRecord>>;
    /// Every conversation record, by (joined_ms, id).
    fn convs(&self) -> Result<Vec<ConvRecord>>;
    /// Remove a conversation with its events and peers.
    fn delete_conv(&self, id: &ConvId) -> Result<()>;

    /// Store an event; false if an event with that id is already stored. A different event at
    /// an already stored (conv, author, seq) is refused with `CoreError::Storage` (equivocation
    /// is never persisted).
    fn insert_event(&self, ev: &StoredEvent) -> Result<bool>;
    /// Store several events (a sync page), each with the semantics of `insert_event`. One
    /// result per event, in order: a refused event does not affect the others. Implementations
    /// may use one transaction (SQLite does: one commit per page instead of one per event).
    /// The default calls `insert_event` for each.
    fn insert_events(&self, evs: &[&StoredEvent]) -> Vec<Result<bool>> { evs.iter().map(|e| self.insert_event(e)).collect() }
    /// One event by id.
    fn event(&self, id: &EventId) -> Result<Option<StoredEvent>>;
    /// Highest seq per author, sorted by author.
    fn heads(&self, conv: &ConvId) -> Result<Vec<(UserId, u64)>>;
    /// Events of one author with `from_seq <= seq <= to_seq`, ascending.
    fn events_range(&self, conv: &ConvId, author: &UserId, from_seq: u64, to_seq: u64) -> Result<Vec<StoredEvent>>;
    /// Every non-Message event, ascending by (author, seq).
    fn state_events(&self, conv: &ConvId) -> Result<Vec<StoredEvent>>;
    /// A page of messages, newest first, strictly older than `before`.
    fn messages(&self, conv: &ConvId, channel: Option<&ChannelId>, before: Option<(u64, EventId)>, limit: usize) -> Result<Vec<StoredEvent>>;
    /// Flag a stored event as deleted.
    fn mark_deleted(&self, conv: &ConvId, target: &EventId) -> Result<()>;

    /// Insert or replace a peer address.
    fn upsert_peer(&self, conv: &ConvId, peer: &PeerRecord) -> Result<()>;
    /// Most recently seen first.
    fn peers(&self, conv: &ConvId, limit: usize) -> Result<Vec<PeerRecord>>;

    /// Read a setting.
    fn get_kv(&self, key: &str) -> Result<Option<Vec<u8>>>;
    /// Write a setting.
    fn set_kv(&self, key: &str, value: &[u8]) -> Result<()>;
}

/// In-memory storage for tests (and the reference for the semantics above).
#[derive(Default)]
pub struct MemStorage {
    inner: parking_lot::Mutex<MemInner>,
}

impl std::fmt::Debug for MemStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock();
        write!(f, "MemStorage({} convs, {} events)", inner.convs.len(), inner.events.len())
    }
}

#[derive(Default)]
struct ConvEvents {
    /// author -> (seq, id), ascending.
    by_author: HashMap<UserId, BTreeSet<(u64, EventId)>>,
    /// (hlc, id) of every Message.
    messages: BTreeSet<(u64, EventId)>,
    /// channel -> (hlc, id) of its Messages.
    by_channel: HashMap<ChannelId, BTreeSet<(u64, EventId)>>,
}

#[derive(Default)]
struct MemInner {
    convs: HashMap<ConvId, ConvRecord>,
    events: HashMap<EventId, StoredEvent>,
    conv_events: HashMap<ConvId, ConvEvents>,
    peers: HashMap<ConvId, HashMap<UserId, PeerRecord>>,
    kv: HashMap<String, Vec<u8>>,
}

impl MemStorage {
    /// Empty storage.
    pub fn new() -> Self { Self::default() }
}

impl Storage for MemStorage {
    fn upsert_conv(&self, c: &ConvRecord) -> Result<()> {
        self.inner.lock().convs.insert(c.id, c.clone());
        Ok(())
    }

    fn conv(&self, id: &ConvId) -> Result<Option<ConvRecord>> { Ok(self.inner.lock().convs.get(id).cloned()) }

    fn convs(&self) -> Result<Vec<ConvRecord>> {
        let mut v: Vec<ConvRecord> = self.inner.lock().convs.values().cloned().collect();
        v.sort_by_key(|c| (c.joined_ms, c.id));
        Ok(v)
    }

    fn delete_conv(&self, id: &ConvId) -> Result<()> {
        let mut inner = self.inner.lock();
        inner.convs.remove(id);
        inner.peers.remove(id);
        if let Some(ce) = inner.conv_events.remove(id) {
            for set in ce.by_author.values() {
                for (_, ev_id) in set {
                    inner.events.remove(ev_id);
                }
            }
        }
        Ok(())
    }

    fn insert_event(&self, ev: &StoredEvent) -> Result<bool> {
        let mut inner = self.inner.lock();
        if inner.events.contains_key(&ev.id) {
            return Ok(false);
        }
        let conv = ev.conv();
        let ce = inner.conv_events.entry(conv).or_default();
        let chain = ce.by_author.entry(ev.event.author).or_default();
        // Same rule as SqliteStorage: a different event at a stored (conv, author, seq) is
        // equivocation and is refused, never stored next to the first one.
        let seq = ev.event.seq;
        if chain.range((seq, EventId([0; 32]))..=(seq, EventId([0xff; 32]))).next().is_some() {
            return Err(CoreError::Storage(format!("conflicting event at seq {seq}")));
        }
        chain.insert((seq, ev.id));
        if let Body::Message { channel, .. } = &ev.event.body {
            ce.messages.insert((ev.event.hlc, ev.id));
            ce.by_channel.entry(*channel).or_default().insert((ev.event.hlc, ev.id));
        }
        inner.events.insert(ev.id, ev.clone());
        Ok(true)
    }

    fn event(&self, id: &EventId) -> Result<Option<StoredEvent>> { Ok(self.inner.lock().events.get(id).cloned()) }

    fn heads(&self, conv: &ConvId) -> Result<Vec<(UserId, u64)>> {
        let inner = self.inner.lock();
        let mut v: Vec<(UserId, u64)> = match inner.conv_events.get(conv) {
            Some(ce) => ce.by_author.iter().filter_map(|(u, set)| set.last().map(|(s, _)| (*u, *s))).collect(),
            None => Vec::new(),
        };
        v.sort();
        Ok(v)
    }

    fn events_range(&self, conv: &ConvId, author: &UserId, from_seq: u64, to_seq: u64) -> Result<Vec<StoredEvent>> {
        if from_seq > to_seq {
            return Ok(Vec::new());
        }
        let inner = self.inner.lock();
        let Some(set) = inner.conv_events.get(conv).and_then(|ce| ce.by_author.get(author)) else { return Ok(Vec::new()) };
        let lo = (from_seq, EventId([0; 32]));
        let hi = (to_seq, EventId([0xff; 32]));
        Ok(set.range(lo..=hi).filter_map(|(_, id)| inner.events.get(id).cloned()).collect())
    }

    fn state_events(&self, conv: &ConvId) -> Result<Vec<StoredEvent>> {
        let inner = self.inner.lock();
        let Some(ce) = inner.conv_events.get(conv) else { return Ok(Vec::new()) };
        let mut authors: Vec<&UserId> = ce.by_author.keys().collect();
        authors.sort();
        let mut out = Vec::new();
        for a in authors {
            for (_, id) in &ce.by_author[a] {
                if let Some(ev) = inner.events.get(id)
                    && !matches!(ev.event.body, Body::Message { .. })
                {
                    out.push(ev.clone());
                }
            }
        }
        Ok(out)
    }

    fn messages(&self, conv: &ConvId, channel: Option<&ChannelId>, before: Option<(u64, EventId)>, limit: usize) -> Result<Vec<StoredEvent>> {
        let inner = self.inner.lock();
        let Some(ce) = inner.conv_events.get(conv) else { return Ok(Vec::new()) };
        let set = match channel {
            Some(c) => match ce.by_channel.get(c) {
                Some(s) => s,
                None => return Ok(Vec::new()),
            },
            None => &ce.messages,
        };
        let iter: Box<dyn Iterator<Item = &(u64, EventId)>> = match before {
            Some(b) => Box::new(set.range(..b).rev()),
            None => Box::new(set.iter().rev()),
        };
        Ok(iter.take(limit).filter_map(|(_, id)| inner.events.get(id).cloned()).collect())
    }

    fn mark_deleted(&self, conv: &ConvId, target: &EventId) -> Result<()> {
        let mut inner = self.inner.lock();
        if let Some(ev) = inner.events.get_mut(target)
            && ev.conv() == *conv
        {
            ev.deleted = true;
        }
        Ok(())
    }

    fn upsert_peer(&self, conv: &ConvId, peer: &PeerRecord) -> Result<()> {
        self.inner.lock().peers.entry(*conv).or_default().insert(peer.user, peer.clone());
        Ok(())
    }

    fn peers(&self, conv: &ConvId, limit: usize) -> Result<Vec<PeerRecord>> {
        let inner = self.inner.lock();
        let mut v: Vec<PeerRecord> = inner.peers.get(conv).map(|m| m.values().cloned().collect()).unwrap_or_default();
        v.sort_by(|a, b| b.last_seen_ms.cmp(&a.last_seen_ms).then(a.user.cmp(&b.user)));
        v.truncate(limit);
        Ok(v)
    }

    fn get_kv(&self, key: &str) -> Result<Option<Vec<u8>>> { Ok(self.inner.lock().kv.get(key).cloned()) }

    fn set_kv(&self, key: &str, value: &[u8]) -> Result<()> {
        self.inner.lock().kv.insert(key.to_string(), value.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PROTOCOL_VERSION;
    use crate::event::{ChannelKind, Event, SignedEvent};
    use crate::keys::Identity;

    fn ev(id: &Identity, conv: ConvId, seq: u64, hlc: u64, body: Body) -> StoredEvent {
        let e = Event { v: PROTOCOL_VERSION, server: conv, author: id.user(), seq, hlc, body };
        StoredEvent::from_signed(SignedEvent::sign(id, &e).unwrap()).unwrap()
    }

    fn msg(id: &Identity, conv: ConvId, seq: u64, hlc: u64, channel: ChannelId) -> StoredEvent {
        ev(id, conv, seq, hlc, Body::Message { channel, text: format!("m{seq}"), attachments: vec![] })
    }

    #[test]
    fn insert_is_idempotent_and_indexed_by_conv() {
        let s = MemStorage::new();
        let a = Identity::generate();
        let genesis = ev(&a, ConvId::default(), 1, 1, Body::Genesis { name: "s".into(), owner: a.user(), nonce: [0; 16] });
        let server = genesis.conv();
        assert!(s.insert_event(&genesis).unwrap());
        assert!(!s.insert_event(&genesis).unwrap());
        // Genesis is found under its own id, not under [0; 32]
        assert_eq!(s.heads(&server).unwrap(), vec![(a.user(), 1)]);
        assert!(s.heads(&ConvId::default()).unwrap().is_empty());
        assert_eq!(s.events_range(&server, &a.user(), 1, 1).unwrap(), vec![genesis.clone()]);
        assert_eq!(s.event(&genesis.id).unwrap(), Some(genesis));
        assert_eq!(s.event(&EventId([9; 32])).unwrap(), None);
    }

    #[test]
    fn heads_ranges_and_state_events() {
        let s = MemStorage::new();
        let a = Identity::generate();
        let b = Identity::generate();
        let conv = ConvId([1; 32]);
        let ch = EventId([2; 32]);
        let a1 = ev(&a, conv, 1, 10, Body::Profile { name: "a".into() });
        let a2 = msg(&a, conv, 2, 20, ch);
        let a3 = ev(&a, conv, 3, 30, Body::Delete { target: a2.id });
        let a4 = ev(&a, conv, 4, 40, Body::ChannelCreate { name: "c".into(), kind: ChannelKind::Text, position: 0 });
        let b1 = msg(&b, conv, 1, 15, ch);
        for e in [&a3, &a1, &b1, &a4, &a2] {
            assert!(s.insert_event(e).unwrap());
        }
        let mut expect = vec![(a.user(), 4), (b.user(), 1)];
        expect.sort();
        assert_eq!(s.heads(&conv).unwrap(), expect);
        let r: Vec<u64> = s.events_range(&conv, &a.user(), 2, 3).unwrap().iter().map(|e| e.event.seq).collect();
        assert_eq!(r, vec![2, 3]);
        assert!(s.events_range(&conv, &a.user(), 5, 9).unwrap().is_empty());
        assert!(s.events_range(&conv, &a.user(), 3, 2).unwrap().is_empty());
        assert!(s.events_range(&conv, &b.user(), 0, u64::MAX).unwrap().len() == 1);
        assert!(s.events_range(&ConvId([5; 32]), &a.user(), 1, 9).unwrap().is_empty());
        let st: Vec<(UserId, u64)> = s.state_events(&conv).unwrap().iter().map(|e| (e.event.author, e.event.seq)).collect();
        // no messages; Delete included; ascending by (author, seq)
        let mut want = vec![(a.user(), 1), (a.user(), 3), (a.user(), 4)];
        want.sort();
        assert_eq!(st, want);
    }

    #[test]
    fn messages_paging_channel_filter_and_deleted() {
        let s = MemStorage::new();
        let a = Identity::generate();
        let conv = ConvId([1; 32]);
        let c1 = EventId([2; 32]);
        let c2 = EventId([3; 32]);
        let mut in_c1 = Vec::new();
        for seq in 1..=10u64 {
            let ch = if seq % 2 == 0 { c2 } else { c1 };
            let m = msg(&a, conv, seq, seq * 100, ch);
            if ch == c1 {
                in_c1.push(m.clone());
            }
            s.insert_event(&m).unwrap();
        }
        s.insert_event(&ev(&a, conv, 11, 1100, Body::Profile { name: "x".into() })).unwrap();
        // all messages, newest first
        let all = s.messages(&conv, None, None, 100).unwrap();
        assert_eq!(all.len(), 10);
        assert!(all.windows(2).all(|w| w[0].order_key() > w[1].order_key()));
        // channel filter and paging
        let p1 = s.messages(&conv, Some(&c1), None, 2).unwrap();
        assert_eq!(p1.iter().map(|e| e.event.seq).collect::<Vec<_>>(), vec![9, 7]);
        let p2 = s.messages(&conv, Some(&c1), Some(p1[1].order_key()), 2).unwrap();
        assert_eq!(p2.iter().map(|e| e.event.seq).collect::<Vec<_>>(), vec![5, 3]);
        let p3 = s.messages(&conv, Some(&c1), Some(p2[1].order_key()), 2).unwrap();
        assert_eq!(p3.iter().map(|e| e.event.seq).collect::<Vec<_>>(), vec![1]);
        assert!(s.messages(&conv, Some(&EventId([4; 32])), None, 5).unwrap().is_empty());
        assert!(s.messages(&conv, None, None, 0).unwrap().is_empty());
        // deleted messages stay listed, flagged
        s.mark_deleted(&conv, &in_c1[0].id).unwrap();
        s.mark_deleted(&ConvId([9; 32]), &in_c1[1].id).unwrap(); // wrong conv: no effect
        s.mark_deleted(&conv, &EventId([7; 32])).unwrap(); // unknown: no effect
        let p = s.messages(&conv, Some(&c1), None, 100).unwrap();
        let flagged: Vec<u64> = p.iter().filter(|e| e.deleted).map(|e| e.event.seq).collect();
        assert_eq!(flagged, vec![1]);
        let kept = s.event(&in_c1[0].id).unwrap().unwrap();
        assert!(kept.deleted);
        assert_eq!(kept.signed, in_c1[0].signed, "signed bytes are kept for sync");
    }

    #[test]
    fn convs_peers_kv_and_delete() {
        let s = MemStorage::new();
        let a = Identity::generate();
        let rec = |b: u8, joined| ConvRecord {
            id: ConvId([b; 32]),
            is_dm: false,
            peer: None,
            name: format!("s{b}"),
            server_secret: Some([b; 32]),
            last_epoch: 0,
            last_host: None,
            joined_ms: joined,
        };
        s.upsert_conv(&rec(2, 20)).unwrap();
        s.upsert_conv(&rec(1, 30)).unwrap();
        s.upsert_conv(&rec(3, 10)).unwrap();
        let mut updated = rec(2, 20);
        updated.name = "renamed".into();
        s.upsert_conv(&updated).unwrap();
        let ids: Vec<u8> = s.convs().unwrap().iter().map(|c| c.id.0[0]).collect();
        assert_eq!(ids, vec![3, 2, 1]);
        assert_eq!(s.conv(&ConvId([2; 32])).unwrap().unwrap().name, "renamed");

        let conv = ConvId([2; 32]);
        for (b, seen) in [(1u8, 100u64), (2, 300), (3, 200)] {
            s.upsert_peer(&conv, &PeerRecord { user: UserId([b; 32]), addr: AddrHint::default(), last_seen_ms: seen }).unwrap();
        }
        s.upsert_peer(&conv, &PeerRecord { user: UserId([1; 32]), addr: AddrHint::default(), last_seen_ms: 400 }).unwrap();
        let p: Vec<u8> = s.peers(&conv, 2).unwrap().iter().map(|p| p.user.0[0]).collect();
        assert_eq!(p, vec![1, 2]);
        assert_eq!(s.peers(&conv, 10).unwrap().len(), 3);
        assert!(s.peers(&ConvId([1; 32]), 10).unwrap().is_empty());

        let m = msg(&a, conv, 1, 1, EventId([5; 32]));
        s.insert_event(&m).unwrap();
        s.delete_conv(&conv).unwrap();
        assert!(s.conv(&conv).unwrap().is_none());
        assert!(s.peers(&conv, 10).unwrap().is_empty());
        assert!(s.event(&m.id).unwrap().is_none());
        assert!(s.heads(&conv).unwrap().is_empty());
        assert!(s.messages(&conv, None, None, 10).unwrap().is_empty());
        assert_eq!(s.convs().unwrap().len(), 2);

        assert_eq!(s.get_kv("k").unwrap(), None);
        s.set_kv("k", b"v1").unwrap();
        s.set_kv("k", b"v2").unwrap();
        assert_eq!(s.get_kv("k").unwrap(), Some(b"v2".to_vec()));
        assert!(format!("{s:?}").starts_with("MemStorage("));
    }

    #[test]
    fn insert_events_reports_per_event() {
        let s = MemStorage::new();
        let a = Identity::generate();
        let conv = ConvId([1; 32]);
        let ch = EventId([2; 32]);
        let m1 = msg(&a, conv, 1, 10, ch);
        let m2 = msg(&a, conv, 2, 20, ch);
        let other2 = ev(&a, conv, 2, 21, Body::Profile { name: "x".into() });
        let res = s.insert_events(&[&m1, &m2, &m1, &other2]);
        assert!(matches!(res[..], [Ok(true), Ok(true), Ok(false), Err(_)]), "{res:?}");
        assert_eq!(s.heads(&conv).unwrap(), vec![(a.user(), 2)]);
        assert_eq!(s.event(&m2.id).unwrap(), Some(m2), "the refused event did not replace the stored one");
        assert!(s.insert_events(&[]).is_empty());
    }

    #[test]
    fn usable_as_shared_trait_object() {
        fn assert_send_sync<T: Send + Sync + ?Sized>(_: &T) {}
        let s: std::sync::Arc<dyn Storage> = std::sync::Arc::new(MemStorage::new());
        assert_send_sync(&s);
        let s2 = s.clone();
        s2.set_kv("x", b"1").unwrap();
        assert_eq!(s.get_kv("x").unwrap(), Some(b"1".to_vec()));
    }
}
