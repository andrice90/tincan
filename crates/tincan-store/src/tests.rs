//! Tests for `SqliteStorage`: every `Storage` method, the app tables, persistence,
//! concurrency, query plans and a 10k-message performance sanity check.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tincan_core::event::{Attachment, Body, ChannelKind, Event, SignedEvent, StoredEvent};
use tincan_core::keys::Identity;
use tincan_core::storage::{ConvRecord, MemStorage, PeerRecord, Storage};
use tincan_core::wire::AddrHint;
use tincan_core::{BlobHash, ChannelId, ConvId, EventId, PROTOCOL_VERSION, UserId};

use crate::app::{UNREAD_ALL, UNREAD_CHANNEL};
use crate::storage_impl::{MSG_ALL, MSG_ALL_BEFORE, MSG_CHANNEL, MSG_CHANNEL_BEFORE, STATE_EVENTS};
use crate::{ClipRecord, ContactRecord, DownloadRecord, SCHEMA_VERSION, SqliteStorage};

/// A 2026 wall clock in unix ms.
const BASE_MS: u64 = 1_790_000_000_000;

fn hlc(ms: u64) -> u64 {
    (BASE_MS + ms) << 16
}

fn make(id: &Identity, server: ConvId, seq: u64, hlc: u64, body: Body) -> StoredEvent {
    let ev = Event { v: PROTOCOL_VERSION, server, author: id.user(), seq, hlc, body };
    let signed = SignedEvent::sign(id, &ev).expect("sign");
    StoredEvent::from_signed(signed).expect("verify")
}

fn genesis(owner: &Identity, name: &str, nonce: u8) -> StoredEvent {
    make(
        owner,
        ConvId([0; 32]),
        1,
        hlc(0),
        Body::Genesis { name: name.into(), owner: owner.user(), nonce: [nonce; 16] },
    )
}

fn msg(id: &Identity, conv: ConvId, seq: u64, hlc: u64, channel: ChannelId, text: &str) -> StoredEvent {
    make(id, conv, seq, hlc, Body::Message { channel, text: text.into(), attachments: Vec::new() })
}

fn chan(n: u8) -> ChannelId {
    EventId([n; 32])
}

fn mem() -> SqliteStorage {
    SqliteStorage::open_in_memory().expect("open in memory")
}

fn ids(evs: &[StoredEvent]) -> Vec<EventId> {
    evs.iter().map(|e| e.id).collect()
}

fn pragma_i64(s: &SqliteStorage, name: &str) -> i64 {
    s.conn().pragma_query_value(None, name, |r| r.get(0)).expect("pragma")
}

// ---- open, pragmas, migrations ------------------------------------------------------

#[test]
fn file_database_uses_wal_and_settings() {
    let dir = tempfile::tempdir().expect("tempdir");
    let s = SqliteStorage::open(dir.path().join("nested").join("tincan.db")).expect("open");
    let mode: String = s.conn().pragma_query_value(None, "journal_mode", |r| r.get(0)).expect("mode");
    assert_eq!(mode.to_ascii_lowercase(), "wal");
    assert_eq!(pragma_i64(&s, "synchronous"), 1, "NORMAL");
    assert_eq!(pragma_i64(&s, "foreign_keys"), 1);
    assert_eq!(pragma_i64(&s, "busy_timeout"), 5000);
    assert_eq!(s.schema_version().expect("version"), SCHEMA_VERSION);
    drop(s);
}

#[test]
fn in_memory_database_has_schema() {
    let s = mem();
    assert_eq!(s.schema_version().expect("version"), SCHEMA_VERSION);
    assert_eq!(pragma_i64(&s, "foreign_keys"), 1);
    assert!(s.convs().expect("convs").is_empty());
}

#[test]
fn reopening_does_not_rerun_migrations() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("t.db");
    {
        let s = SqliteStorage::open(&path).expect("open");
        s.set_kv("k", b"v").expect("set");
    }
    let s = SqliteStorage::open(&path).expect("reopen");
    assert_eq!(s.schema_version().expect("version"), SCHEMA_VERSION);
    assert_eq!(s.get_kv("k").expect("get").as_deref(), Some(&b"v"[..]));
    drop(s);
}

#[test]
fn refuses_database_from_newer_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("t.db");
    drop(SqliteStorage::open(&path).expect("open"));
    {
        let raw = rusqlite::Connection::open(&path).expect("raw open");
        raw.pragma_update(None, "user_version", i64::from(SCHEMA_VERSION) + 1).expect("bump");
    }
    let err = SqliteStorage::open(&path).expect_err("must refuse");
    assert!(err.to_string().contains("newer"), "{err}");
}

#[test]
fn simultaneous_first_open_migrates_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("race.db");
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let openers: Vec<_> = (0..4)
        .map(|_| {
            let (path, barrier) = (path.clone(), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                let s = SqliteStorage::open(&path).expect("concurrent open");
                assert_eq!(s.schema_version().expect("version"), SCHEMA_VERSION);
            })
        })
        .collect();
    for o in openers {
        o.join().expect("opener");
    }
}

// ---- conversations ------------------------------------------------------------------

#[test]
fn conv_upsert_get_list_delete() {
    let s = mem();
    let a = Identity::generate();
    let b = Identity::generate();
    let dm = ConvRecord {
        id: ConvId::dm(&a.user(), &b.user()),
        is_dm: true,
        peer: Some(b.user()),
        name: "Bea".into(),
        server_secret: None,
        last_epoch: 0,
        last_host: None,
        joined_ms: 20,
    };
    let mut server = ConvRecord {
        id: ConvId([5; 32]),
        is_dm: false,
        peer: None,
        name: "Friends".into(),
        server_secret: Some([9; 32]),
        last_epoch: 3,
        last_host: Some(a.user()),
        joined_ms: 10,
    };
    s.upsert_conv(&dm).expect("dm");
    s.upsert_conv(&server).expect("server");
    assert_eq!(s.conv(&dm.id).expect("get"), Some(dm.clone()));
    assert_eq!(s.conv(&ConvId([1; 32])).expect("get"), None);
    assert_eq!(s.convs().expect("list"), vec![server.clone(), dm.clone()], "ordered by joined_ms");

    server.name = "Renamed".into();
    server.last_epoch = u64::from(u32::MAX) + 7;
    server.last_host = Some(b.user());
    server.server_secret = None;
    s.upsert_conv(&server).expect("update");
    assert_eq!(s.conv(&server.id).expect("get"), Some(server.clone()));
    assert_eq!(s.convs().expect("list").len(), 2);

    // delete_conv removes the record and everything stored under it, nothing else.
    let ch = chan(1);
    s.insert_event(&msg(&a, server.id, 1, hlc(1), ch, "in server")).expect("insert");
    s.insert_event(&msg(&a, dm.id, 1, hlc(2), ch, "in dm")).expect("insert");
    let peer = PeerRecord { user: b.user(), addr: AddrHint::default(), last_seen_ms: 5 };
    s.upsert_peer(&server.id, &peer).expect("peer");
    s.upsert_peer(&dm.id, &peer).expect("peer");
    s.set_read(&server.id, Some(&ch), hlc(1)).expect("read");
    s.set_read(&dm.id, None, hlc(2)).expect("read");

    s.delete_conv(&server.id).expect("delete");
    assert_eq!(s.conv(&server.id).expect("get"), None);
    assert!(s.heads(&server.id).expect("heads").is_empty());
    assert!(s.peers(&server.id, 10).expect("peers").is_empty());
    assert_eq!(s.last_read(&server.id, Some(&ch)).expect("marker"), None);
    assert_eq!(s.convs().expect("list"), vec![dm.clone()]);
    assert_eq!(s.heads(&dm.id).expect("heads"), vec![(a.user(), 1)]);
    assert_eq!(s.peers(&dm.id, 10).expect("peers").len(), 1);
    assert_eq!(s.last_read(&dm.id, None).expect("marker"), Some(hlc(2)));

    s.delete_conv(&ConvId([77; 32])).expect("unknown id is a no-op");
}

// ---- events -------------------------------------------------------------------------

#[test]
fn insert_is_idempotent_and_roundtrips() {
    let s = mem();
    let a = Identity::generate();
    let conv = ConvId([1; 32]);
    let ev = make(
        &a,
        conv,
        1,
        hlc(1),
        Body::Message {
            channel: chan(2),
            text: "hello".into(),
            attachments: vec![Attachment { hash: BlobHash([3; 32]), size: 42, name: "a.png".into(), mime: "image/png".into() }],
        },
    );
    assert!(s.insert_event(&ev).expect("first"));
    assert!(!s.insert_event(&ev).expect("duplicate"));
    let back = s.event(&ev.id).expect("get").expect("present");
    assert_eq!(back, ev);
    back.signed.verify().expect("stored bytes and signature are intact");
    assert_eq!(s.event(&EventId([0; 32])).expect("get"), None);
    assert_eq!(s.insert_events_batch(std::slice::from_ref(&ev)).expect("batch"), 0);
}

#[test]
fn conflicting_author_seq_is_refused() {
    let s = mem();
    let a = Identity::generate();
    let conv = ConvId([1; 32]);
    let first = msg(&a, conv, 1, hlc(1), chan(1), "one");
    let other = msg(&a, conv, 1, hlc(2), chan(1), "equivocation");
    assert!(s.insert_event(&first).expect("insert"));
    assert!(s.insert_event(&other).is_err());
    assert_eq!(s.event(&other.id).expect("get"), None);

    // A batch containing a conflict keeps nothing.
    let fresh = msg(&a, conv, 2, hlc(3), chan(1), "two");
    assert!(s.insert_events_batch(&[fresh.clone(), other]).is_err());
    assert_eq!(s.event(&fresh.id).expect("get"), None);
    assert_eq!(s.heads(&conv).expect("heads"), vec![(a.user(), 1)]);
}

#[test]
fn genesis_is_stored_under_its_own_id() {
    let s = mem();
    let owner = Identity::generate();
    let g1 = genesis(&owner, "one", 1);
    let g2 = genesis(&owner, "two", 2);
    assert_ne!(g1.id, g2.id);
    assert!(s.insert_event(&g1).expect("g1"));
    assert!(s.insert_event(&g2).expect("g2: same owner, seq 1, different server"));
    let s1 = ConvId::from_event(&g1.id);
    let s2 = ConvId::from_event(&g2.id);
    assert_eq!(s.heads(&s1).expect("heads"), vec![(owner.user(), 1)]);
    assert_eq!(s.heads(&s2).expect("heads"), vec![(owner.user(), 1)]);
    assert!(s.heads(&ConvId([0; 32])).expect("heads").is_empty());
    assert_eq!(ids(&s.state_events(&s1).expect("state")), vec![g1.id]);
    assert_eq!(ids(&s.events_range(&s1, &owner.user(), 1, 1).expect("range")), vec![g1.id]);
    s.mark_deleted(&s1, &g1.id).expect("mark");
    assert!(s.event(&g1.id).expect("get").expect("present").deleted);
}

#[test]
fn heads_are_highest_seq_per_author() {
    let s = mem();
    let conv = ConvId([1; 32]);
    let other = ConvId([2; 32]);
    let a = Identity::generate();
    let b = Identity::generate();
    for seq in 1..=5 {
        s.insert_event(&msg(&a, conv, seq, hlc(seq), chan(1), "a")).expect("a");
    }
    for seq in 1..=3 {
        s.insert_event(&msg(&b, conv, seq, hlc(100 + seq), chan(1), "b")).expect("b");
    }
    s.insert_event(&msg(&b, other, 1, hlc(1), chan(1), "elsewhere")).expect("other");
    let mut expected = vec![(a.user(), 5), (b.user(), 3)];
    expected.sort();
    assert_eq!(s.heads(&conv).expect("heads"), expected);
    assert_eq!(s.heads(&other).expect("heads"), vec![(b.user(), 1)]);
    assert!(s.heads(&ConvId([3; 32])).expect("heads").is_empty());
}

#[test]
fn events_range_is_inclusive_and_ascending() {
    let s = mem();
    let conv = ConvId([1; 32]);
    let a = Identity::generate();
    let b = Identity::generate();
    let evs: Vec<StoredEvent> = (1..=10).map(|seq| msg(&a, conv, seq, hlc(seq), chan(1), "x")).collect();
    // Insert out of order; reads must still be ascending.
    for ev in evs.iter().rev() {
        s.insert_event(ev).expect("insert");
    }
    s.insert_event(&msg(&b, conv, 1, hlc(50), chan(1), "b")).expect("b");

    assert_eq!(ids(&s.events_range(&conv, &a.user(), 3, 6).expect("range")), ids(&evs[2..6]));
    assert_eq!(ids(&s.events_range(&conv, &a.user(), 1, u64::MAX).expect("open end")), ids(&evs));
    assert_eq!(ids(&s.events_range(&conv, &a.user(), 10, 10).expect("single")), ids(&evs[9..]));
    assert!(s.events_range(&conv, &a.user(), 7, 3).expect("reversed").is_empty());
    assert!(s.events_range(&conv, &a.user(), 11, 20).expect("beyond").is_empty());
    assert!(s.events_range(&conv, &a.user(), u64::MAX, u64::MAX).expect("max").is_empty());
    assert!(s.events_range(&ConvId([2; 32]), &a.user(), 1, 10).expect("other conv").is_empty());
    assert_eq!(s.events_range(&conv, &b.user(), 0, 100).expect("b").len(), 1);
}

#[test]
fn messages_page_newest_first_with_exclusive_cursor() {
    let s = mem();
    let conv = ConvId([1; 32]);
    let (ch1, ch2) = (chan(1), chan(2));
    let a = Identity::generate();
    let b = Identity::generate();
    let c = Identity::generate();
    let mut all = vec![
        msg(&a, conv, 1, hlc(10), ch1, "a1"),
        msg(&b, conv, 1, hlc(10), ch1, "b1 (same hlc as a1)"),
        msg(&a, conv, 2, hlc(20), ch2, "a2 other channel"),
        msg(&b, conv, 2, hlc(30), ch1, "b2"),
        msg(&a, conv, 3, hlc(40), ch1, "a3"),
        msg(&c, conv, 2, hlc(15), ch1, "c2"),
    ];
    let profile = make(&c, conv, 1, hlc(5), Body::Profile { name: "Cee".into() });
    s.insert_event(&profile).expect("profile");
    for ev in &all {
        s.insert_event(ev).expect("insert");
    }
    s.insert_event(&msg(&a, ConvId([2; 32]), 1, hlc(99), ch1, "other conv")).expect("other");

    all.sort_by_key(|e| std::cmp::Reverse(e.order_key()));
    let in_ch1: Vec<EventId> = all.iter().filter(|e| e.event.body.channel() == Some(ch1)).map(|e| e.id).collect();
    assert_eq!(in_ch1.len(), 5);

    // Full page, one channel.
    assert_eq!(ids(&s.messages(&conv, Some(&ch1), None, 100).expect("page")), in_ch1);
    // All channels of the conversation, state events excluded.
    assert_eq!(ids(&s.messages(&conv, None, None, 100).expect("all")), ids(&all));
    let in_ch2: Vec<EventId> = all.iter().filter(|e| e.event.body.channel() == Some(ch2)).map(|e| e.id).collect();
    assert_eq!(in_ch2.len(), 1);
    assert_eq!(ids(&s.messages(&conv, Some(&ch2), None, 100).expect("ch2")), in_ch2);
    assert!(s.messages(&conv, Some(&chan(9)), None, 100).expect("empty channel").is_empty());

    // Paging by 2 walks the same order with no overlap, including across the hlc tie.
    let mut walked = Vec::new();
    let mut cursor = None;
    loop {
        let page = s.messages(&conv, Some(&ch1), cursor, 2).expect("page");
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 2);
        cursor = page.last().map(StoredEvent::order_key);
        walked.extend(ids(&page));
    }
    assert_eq!(walked, in_ch1);

    // Same for the all-channels path.
    let mut walked = Vec::new();
    let mut cursor = None;
    loop {
        let page = s.messages(&conv, None, cursor, 4).expect("page");
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(StoredEvent::order_key);
        walked.extend(ids(&page));
    }
    assert_eq!(walked, ids(&all));

    // The cursor is strictly exclusive.
    let newest = &all[0];
    let after_newest = s.messages(&conv, None, Some(newest.order_key()), 100).expect("page");
    assert_eq!(ids(&after_newest), ids(&all[1..]));

    // Limits and extreme cursors.
    assert!(s.messages(&conv, Some(&ch1), None, 0).expect("zero").is_empty());
    assert_eq!(s.messages(&conv, Some(&ch1), None, usize::MAX).expect("max").len(), 5);
    let top = Some((u64::MAX, EventId([0xff; 32])));
    assert_eq!(ids(&s.messages(&conv, Some(&ch1), top, 100).expect("max cursor")), in_ch1);
    assert!(s.messages(&conv, None, Some((0, EventId([0; 32]))), 100).expect("min cursor").is_empty());

    // Deleted messages are still returned, flagged.
    let victim = in_ch1[1];
    s.mark_deleted(&conv, &victim).expect("mark");
    let page = s.messages(&conv, Some(&ch1), None, 100).expect("page");
    assert_eq!(ids(&page), in_ch1);
    for ev in &page {
        assert_eq!(ev.deleted, ev.id == victim);
    }
}

#[test]
fn state_events_exclude_messages_and_order_by_author_seq() {
    let s = mem();
    let owner = Identity::generate();
    let member = Identity::generate();
    let g = genesis(&owner, "srv", 1);
    let conv = ConvId::from_event(&g.id);
    let create = make(&owner, conv, 2, hlc(2), Body::ChannelCreate { name: "general".into(), kind: ChannelKind::Text, position: 0 });
    let ch = create.id;
    let owner_msg = msg(&owner, conv, 3, hlc(3), ch, "hi");
    let owner_del = make(&owner, conv, 4, hlc(4), Body::Delete { target: owner_msg.id });
    let admit = make(&owner, conv, 5, hlc(5), Body::MemberAdmit { user: member.user(), invite_pk: [4; 32], from_seq: 1 });
    let profile = make(&member, conv, 1, hlc(6), Body::Profile { name: "M".into() });
    let member_msg = msg(&member, conv, 2, hlc(7), ch, "yo");
    let member_del = make(&member, conv, 3, hlc(8), Body::Delete { target: member_msg.id });
    let leave = make(&member, conv, 4, hlc(9), Body::MemberLeave { last_seq: 4 });
    let all = [g, create, owner_msg, owner_del, admit, profile, member_msg, member_del, leave];
    // Insert in reverse to prove ordering comes from the query.
    for ev in all.iter().rev() {
        s.insert_event(ev).expect("insert");
    }
    let mut expected: Vec<&StoredEvent> = all.iter().filter(|e| !matches!(e.event.body, Body::Message { .. })).collect();
    expected.sort_by_key(|e| (e.event.author, e.event.seq));
    let got = s.state_events(&conv).expect("state");
    assert_eq!(got.len(), 7);
    assert_eq!(ids(&got), expected.iter().map(|e| e.id).collect::<Vec<_>>());
    assert!(got.iter().any(|e| matches!(e.event.body, Body::Delete { .. })), "deletes are included");
    assert!(s.state_events(&ConvId([3; 32])).expect("empty").is_empty());
}

#[test]
fn mark_deleted_is_scoped_to_the_conversation() {
    let s = mem();
    let a = Identity::generate();
    let conv = ConvId([1; 32]);
    let ev = msg(&a, conv, 1, hlc(1), chan(1), "secret");
    s.insert_event(&ev).expect("insert");
    s.mark_deleted(&ConvId([2; 32]), &ev.id).expect("wrong conv");
    assert!(!s.event(&ev.id).expect("get").expect("present").deleted);
    s.mark_deleted(&conv, &EventId([9; 32])).expect("unknown target is a no-op");
    s.mark_deleted(&conv, &ev.id).expect("mark");
    s.mark_deleted(&conv, &ev.id).expect("again");
    let back = s.event(&ev.id).expect("get").expect("present");
    assert!(back.deleted);
    assert_eq!(back.signed, ev.signed, "signed bytes are kept for sync");
    assert!(s.events_range(&conv, &a.user(), 1, 1).expect("range")[0].deleted);
}

/// Same inputs into `SqliteStorage` and core's reference `MemStorage`: every read must
/// agree (both impls follow the semantics in `tincan_core::storage`).
#[test]
fn matches_the_reference_mem_storage() {
    let sql = mem();
    let reference = MemStorage::new();
    let stores: [&dyn Storage; 2] = [&sql, &reference];

    let owner = Identity::generate();
    let members = [Identity::generate(), Identity::generate()];
    let g = genesis(&owner, "srv", 3);
    let conv = ConvId::from_event(&g.id);
    let general = make(&owner, conv, 2, hlc(1), Body::ChannelCreate { name: "general".into(), kind: ChannelKind::Text, position: 0 });
    let random = make(&owner, conv, 3, hlc(2), Body::ChannelCreate { name: "random".into(), kind: ChannelKind::Text, position: 1 });
    let mut events = vec![g.clone(), general.clone(), random.clone()];
    let mut next_seq = [4u64, 1, 1];
    for i in 0..60u64 {
        let who = (i % 3) as usize;
        let id = if who == 0 { &owner } else { &members[who - 1] };
        let seq = next_seq[who];
        next_seq[who] += 1;
        // Coarse clock: many hlc ties, broken by id.
        let t = hlc(10 + i / 4);
        let ev = if seq == 1 {
            make(id, conv, seq, t, Body::Profile { name: format!("member {who}") })
        } else if i % 11 == 0 {
            let target = events[events.len() - 2].id;
            make(id, conv, seq, t, Body::Delete { target })
        } else {
            let ch = if i % 2 == 0 { general.id } else { random.id };
            msg(id, conv, seq, t, ch, &format!("m{i}"))
        };
        events.push(ev);
    }
    for st in stores {
        for ev in &events {
            assert!(st.insert_event(ev).expect("insert"));
            assert!(!st.insert_event(ev).expect("duplicate"));
        }
        for victim in [events[10].id, events[25].id, g.id] {
            st.mark_deleted(&conv, &victim).expect("mark");
        }
        st.mark_deleted(&ConvId([1; 32]), &events[30].id).expect("wrong conv");
    }

    let users = [owner.user(), members[0].user(), members[1].user()];
    assert_eq!(sql.heads(&conv).expect("heads"), reference.heads(&conv).expect("heads"));
    assert_eq!(sql.state_events(&conv).expect("state"), reference.state_events(&conv).expect("state"));
    for u in &users {
        for (from, to) in [(1, u64::MAX), (2, 5), (7, 3), (0, 0)] {
            assert_eq!(
                sql.events_range(&conv, u, from, to).expect("range"),
                reference.events_range(&conv, u, from, to).expect("range"),
            );
        }
    }
    for ev in &events {
        assert_eq!(sql.event(&ev.id).expect("event"), reference.event(&ev.id).expect("event"));
    }
    for channel in [Some(&general.id), Some(&random.id), None, Some(&chan(99))] {
        for limit_n in [1, 7, 1000] {
            let mut cursor = None;
            loop {
                let a = sql.messages(&conv, channel, cursor, limit_n).expect("page");
                let b = reference.messages(&conv, channel, cursor, limit_n).expect("page");
                assert_eq!(a, b, "channel {channel:?} limit {limit_n} cursor {cursor:?}");
                match a.last() {
                    Some(last) => cursor = Some(last.order_key()),
                    None => break,
                }
            }
        }
    }

    for st in stores {
        for (i, u) in users.iter().enumerate() {
            let p = PeerRecord { user: *u, addr: AddrHint::default(), last_seen_ms: 50 + (i as u64 % 2) };
            st.upsert_peer(&conv, &p).expect("peer");
        }
        st.set_kv("k", b"v").expect("kv");
    }
    for limit_n in [0, 2, 10] {
        assert_eq!(sql.peers(&conv, limit_n).expect("peers"), reference.peers(&conv, limit_n).expect("peers"));
    }
    assert_eq!(sql.get_kv("k").expect("kv"), reference.get_kv("k").expect("kv"));

    let record = ConvRecord {
        id: conv,
        is_dm: false,
        peer: None,
        name: "srv".into(),
        server_secret: Some([2; 32]),
        last_epoch: 1,
        last_host: None,
        joined_ms: 5,
    };
    for st in stores {
        st.upsert_conv(&record).expect("conv");
        st.delete_conv(&conv).expect("delete");
    }
    assert_eq!(sql.convs().expect("convs"), reference.convs().expect("convs"));
    assert_eq!(sql.heads(&conv).expect("heads"), reference.heads(&conv).expect("heads"));
    assert_eq!(sql.peers(&conv, 10).expect("peers"), reference.peers(&conv, 10).expect("peers"));
    assert!(sql.heads(&conv).expect("heads").is_empty());
}

// ---- peers and kv -------------------------------------------------------------------

#[test]
fn peers_most_recent_first() {
    let s = mem();
    let conv = ConvId([1; 32]);
    let users: Vec<UserId> = (0..4).map(|_| Identity::generate().user()).collect();
    let addr = |n: u8| AddrHint {
        relay: Some(format!("https://relay{n}.example/")),
        direct: vec![format!("192.168.1.{n}:4433").parse().expect("addr"), "[::1]:9".parse().expect("addr")],
    };
    let seen = [100u64, 300, 200, 300];
    for (i, u) in users.iter().enumerate() {
        s.upsert_peer(&conv, &PeerRecord { user: *u, addr: addr(i as u8), last_seen_ms: seen[i] }).expect("peer");
    }
    s.upsert_peer(&ConvId([2; 32]), &PeerRecord { user: users[0], addr: AddrHint::default(), last_seen_ms: 999 })
        .expect("other conv");

    let got = s.peers(&conv, 10).expect("peers");
    let order: Vec<UserId> = got.iter().map(|p| p.user).collect();
    let (first, second) = if users[1] < users[3] { (users[1], users[3]) } else { (users[3], users[1]) };
    assert_eq!(order, vec![first, second, users[2], users[0]], "last_seen desc, ties by user");
    assert_eq!(got[3].addr, addr(0));
    assert_eq!(s.peers(&conv, 2).expect("limit").len(), 2);
    assert!(s.peers(&conv, 0).expect("zero").is_empty());

    // Upsert replaces address and last_seen.
    s.upsert_peer(&conv, &PeerRecord { user: users[0], addr: AddrHint::default(), last_seen_ms: 1_000 }).expect("update");
    let got = s.peers(&conv, usize::MAX).expect("peers");
    assert_eq!(got.len(), 4);
    assert_eq!(got[0], PeerRecord { user: users[0], addr: AddrHint::default(), last_seen_ms: 1_000 });
    assert_eq!(s.peers(&ConvId([2; 32]), 10).expect("other").len(), 1);
}

#[test]
fn kv_set_get_overwrite() {
    let s = mem();
    assert_eq!(s.get_kv("missing").expect("get"), None);
    s.set_kv("identity", &[0, 1, 2, 255]).expect("set");
    assert_eq!(s.get_kv("identity").expect("get"), Some(vec![0, 1, 2, 255]));
    s.set_kv("identity", b"replaced").expect("overwrite");
    assert_eq!(s.get_kv("identity").expect("get"), Some(b"replaced".to_vec()));
    s.set_kv("empty", &[]).expect("empty value");
    assert_eq!(s.get_kv("empty").expect("get"), Some(Vec::new()));
}

// ---- app tables ---------------------------------------------------------------------

#[test]
fn settings_json_roundtrip() {
    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Prefs {
        theme: String,
        volume: f32,
    }
    let s = mem();
    assert_eq!(s.get_settings_json().expect("get"), None);
    assert_eq!(s.get_settings::<Prefs>().expect("typed"), None);
    s.set_settings_json(r#"{"theme":"dark","volume":1.5}"#).expect("set");
    assert_eq!(s.get_settings_json().expect("get").as_deref(), Some(r#"{"theme":"dark","volume":1.5}"#));
    assert_eq!(s.get_settings::<Prefs>().expect("typed"), Some(Prefs { theme: "dark".into(), volume: 1.5 }));
    assert!(s.set_settings_json("{not json").is_err());
    assert_eq!(s.get_settings::<Prefs>().expect("unchanged").map(|p| p.theme), Some("dark".into()));
    s.set_settings(&Prefs { theme: "light".into(), volume: 0.5 }).expect("typed set");
    assert_eq!(s.get_settings::<Prefs>().expect("typed"), Some(Prefs { theme: "light".into(), volume: 0.5 }));
    s.set_settings_json("[1,2]").expect("any JSON value");
    assert!(s.get_settings::<Prefs>().is_err(), "shape mismatch is reported");
}

#[test]
fn clip_library() {
    let s = mem();
    let clip = |n: u8, added: u64| ClipRecord {
        hash: BlobHash([n; 32]),
        name: format!("clip {n}"),
        duration_ms: 1_500 * u32::from(n),
        size: 10_000 * u64::from(n),
        added_ms: added,
    };
    assert!(s.list_clips().expect("list").is_empty());
    assert!(s.add_clip(&clip(2, 50)).expect("add"));
    assert!(s.add_clip(&clip(1, 70)).expect("add"));
    assert!(s.add_clip(&clip(3, 60)).expect("add"));
    let mut dup = clip(2, 99);
    dup.name = "other name".into();
    assert!(!s.add_clip(&dup).expect("duplicate"));
    assert_eq!(s.list_clips().expect("list"), vec![clip(2, 50), clip(3, 60), clip(1, 70)]);
    assert_eq!(s.get_clip(&BlobHash([2; 32])).expect("get"), Some(clip(2, 50)));

    assert!(s.rename_clip(&BlobHash([3; 32]), "airhorn").expect("rename"));
    assert!(!s.rename_clip(&BlobHash([8; 32]), "nope").expect("rename unknown"));
    assert_eq!(s.get_clip(&BlobHash([3; 32])).expect("get").map(|c| c.name), Some("airhorn".into()));

    assert!(s.remove_clip(&BlobHash([2; 32])).expect("remove"));
    assert!(!s.remove_clip(&BlobHash([2; 32])).expect("remove again"));
    assert_eq!(s.list_clips().expect("list").len(), 2);
    assert_eq!(s.get_clip(&BlobHash([2; 32])).expect("get"), None);
}

#[test]
fn contacts_upsert_keeps_first_added() {
    let s = mem();
    let a = Identity::generate().user();
    let b = Identity::generate().user();
    s.upsert_contact(&ContactRecord { user: a, name: "Al".into(), added_ms: 20 }).expect("a");
    s.upsert_contact(&ContactRecord { user: b, name: "Bo".into(), added_ms: 10 }).expect("b");
    s.upsert_contact(&ContactRecord { user: a, name: "Alice".into(), added_ms: 999 }).expect("rename");
    assert_eq!(
        s.list_contacts().expect("list"),
        vec![
            ContactRecord { user: b, name: "Bo".into(), added_ms: 10 },
            ContactRecord { user: a, name: "Alice".into(), added_ms: 20 },
        ]
    );
    assert_eq!(s.get_contact(&a).expect("get").map(|c| c.name), Some("Alice".into()));
    assert!(s.remove_contact(&b).expect("remove"));
    assert!(!s.remove_contact(&b).expect("again"));
    assert_eq!(s.get_contact(&b).expect("get"), None);
}

#[test]
fn read_markers_and_unread() {
    let s = mem();
    let me = Identity::generate();
    let other = Identity::generate();
    let conv = ConvId([1; 32]);
    let (ch1, ch2) = (chan(1), chan(2));

    assert!(!s.unread(&conv, Some(&ch1), &me.user()).expect("empty"));
    assert_eq!(s.last_read(&conv, Some(&ch1)).expect("marker"), None);

    // My own messages never count.
    s.insert_event(&msg(&me, conv, 1, hlc(5), ch1, "mine")).expect("insert");
    assert!(!s.unread(&conv, Some(&ch1), &me.user()).expect("own"));

    s.insert_event(&msg(&other, conv, 1, hlc(10), ch1, "theirs")).expect("insert");
    assert!(s.unread(&conv, Some(&ch1), &me.user()).expect("unread"));
    assert!(!s.unread(&conv, Some(&ch2), &me.user()).expect("other channel"));
    assert!(s.unread(&conv, None, &me.user()).expect("conversation-wide"));

    s.set_read(&conv, Some(&ch1), hlc(10)).expect("read");
    assert!(!s.unread(&conv, Some(&ch1), &me.user()).expect("read up to it"));
    s.set_read(&conv, Some(&ch1), hlc(1)).expect("older");
    assert_eq!(s.last_read(&conv, Some(&ch1)).expect("marker"), Some(hlc(10)), "never moves backwards");
    // The channel marker does not touch the conversation-wide one.
    assert_eq!(s.last_read(&conv, None).expect("marker"), None);

    let late = msg(&other, conv, 2, hlc(20), ch1, "new");
    s.insert_event(&late).expect("insert");
    assert!(s.unread(&conv, Some(&ch1), &me.user()).expect("new message"));
    s.mark_deleted(&conv, &late.id).expect("delete");
    assert!(!s.unread(&conv, Some(&ch1), &me.user()).expect("deleted does not count"));

    // DM: conversation-wide marker over all messages.
    let dm = ConvId::dm(&me.user(), &other.user());
    let dm_ch = chan(0);
    s.insert_event(&msg(&other, dm, 1, hlc(30), dm_ch, "dm")).expect("insert");
    assert!(s.unread(&dm, None, &me.user()).expect("dm unread"));
    s.set_read(&dm, None, hlc(30)).expect("read");
    assert!(!s.unread(&dm, None, &me.user()).expect("dm read"));
    assert_eq!(s.last_read(&dm, None).expect("marker"), Some(hlc(30)));
    s.set_read(&dm, None, hlc(31)).expect("forward");
    assert_eq!(s.last_read(&dm, None).expect("marker"), Some(hlc(31)));
}

#[test]
fn downloads_upsert_get_list() {
    let s = mem();
    let d = |n: u8, state: &str| DownloadRecord {
        hash: BlobHash([n; 32]),
        name: format!("file{n}.zip"),
        size: u64::from(n) << 33,
        state: state.into(),
    };
    s.upsert_download(&d(2, "queued")).expect("2");
    s.upsert_download(&d(1, "downloading")).expect("1");
    s.upsert_download(&d(2, "complete")).expect("update keeps position");
    assert_eq!(s.list_downloads().expect("list"), vec![d(2, "complete"), d(1, "downloading")]);
    assert_eq!(s.get_download(&BlobHash([1; 32])).expect("get"), Some(d(1, "downloading")));
    assert_eq!(s.get_download(&BlobHash([7; 32])).expect("get"), None);
    assert!(s.remove_download(&BlobHash([2; 32])).expect("remove"));
    assert!(!s.remove_download(&BlobHash([2; 32])).expect("again"));
    assert_eq!(s.list_downloads().expect("list"), vec![d(1, "downloading")]);
}

// ---- persistence and concurrency ----------------------------------------------------

#[test]
fn everything_survives_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("profile.db");
    let owner = Identity::generate();
    let friend = Identity::generate().user();
    let g = genesis(&owner, "srv", 1);
    let conv = ConvId::from_event(&g.id);
    let ch = chan(4);
    let m1 = msg(&owner, conv, 2, hlc(2), ch, "persist me");
    let m2 = msg(&owner, conv, 3, hlc(3), ch, "and delete me");
    let record = ConvRecord {
        id: conv,
        is_dm: false,
        peer: None,
        name: "srv".into(),
        server_secret: Some([1; 32]),
        last_epoch: 4,
        last_host: Some(owner.user()),
        joined_ms: 77,
    };
    let peer = PeerRecord { user: friend, addr: AddrHint { relay: Some("https://r/".into()), direct: vec![] }, last_seen_ms: 9 };
    let clip = ClipRecord { hash: BlobHash([5; 32]), name: "boom".into(), duration_ms: 900, size: 1234, added_ms: 3 };
    let contact = ContactRecord { user: friend, name: "Friend".into(), added_ms: 8 };
    let download = DownloadRecord { hash: BlobHash([6; 32]), name: "x.bin".into(), size: 10, state: "queued".into() };
    {
        let s = SqliteStorage::open(&path).expect("open");
        s.upsert_conv(&record).expect("conv");
        assert_eq!(s.insert_events_batch(&[g.clone(), m1.clone(), m2.clone()]).expect("batch"), 3);
        s.mark_deleted(&conv, &m2.id).expect("mark");
        s.upsert_peer(&conv, &peer).expect("peer");
        s.set_kv("k", b"v").expect("kv");
        s.set_settings_json(r#"{"theme":"dark"}"#).expect("settings");
        s.add_clip(&clip).expect("clip");
        s.upsert_contact(&contact).expect("contact");
        s.set_read(&conv, Some(&ch), hlc(2)).expect("read");
        s.upsert_download(&download).expect("download");
    }
    let s = SqliteStorage::open(&path).expect("reopen");
    assert_eq!(s.convs().expect("convs"), vec![record]);
    assert_eq!(s.heads(&conv).expect("heads"), vec![(owner.user(), 3)]);
    assert_eq!(s.event(&m1.id).expect("get"), Some(m1.clone()));
    let page = s.messages(&conv, Some(&ch), None, 10).expect("page");
    assert_eq!(ids(&page), vec![m2.id, m1.id]);
    assert!(page[0].deleted && !page[1].deleted);
    assert_eq!(ids(&s.state_events(&conv).expect("state")), vec![g.id]);
    assert_eq!(s.peers(&conv, 5).expect("peers"), vec![peer]);
    assert_eq!(s.get_kv("k").expect("kv"), Some(b"v".to_vec()));
    assert_eq!(s.get_settings_json().expect("settings").as_deref(), Some(r#"{"theme":"dark"}"#));
    assert_eq!(s.list_clips().expect("clips"), vec![clip]);
    assert_eq!(s.list_contacts().expect("contacts"), vec![contact]);
    assert_eq!(s.last_read(&conv, Some(&ch)).expect("marker"), Some(hlc(2)));
    assert!(!s.unread(&conv, Some(&ch), &friend).expect("unread"), "m1 read, m2 deleted");
    assert_eq!(s.list_downloads().expect("downloads"), vec![download]);
    drop(s);
}

#[test]
fn concurrent_threads_share_one_store() {
    const THREADS: u64 = 8;
    const PER_THREAD: u64 = 100;
    let s = Arc::new(mem());
    let as_trait: Arc<dyn Storage> = s.clone();
    let conv = ConvId([3; 32]);
    let ch = chan(1);
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let st = Arc::clone(&as_trait);
            let app = Arc::clone(&s);
            std::thread::spawn(move || {
                let id = Identity::generate();
                for seq in 1..=PER_THREAD {
                    let ev = msg(&id, conv, seq, hlc(t * 10_000 + seq), ch, "concurrent");
                    assert!(st.insert_event(&ev).expect("insert"));
                    if seq % 10 == 0 {
                        st.heads(&conv).expect("heads");
                        st.messages(&conv, Some(&ch), None, 20).expect("page");
                        st.set_kv(&format!("t{t}"), &seq.to_le_bytes()).expect("kv");
                        app.set_read(&conv, Some(&ch), hlc(seq)).expect("read");
                    }
                }
                id.user()
            })
        })
        .collect();
    let mut users: Vec<UserId> = handles.into_iter().map(|h| h.join().expect("thread")).collect();
    users.sort();
    let expected: Vec<(UserId, u64)> = users.iter().map(|u| (*u, PER_THREAD)).collect();
    assert_eq!(s.heads(&conv).expect("heads"), expected);
    assert_eq!(s.messages(&conv, None, None, usize::MAX).expect("all").len() as u64, THREADS * PER_THREAD);
    for t in 0..THREADS {
        assert_eq!(s.get_kv(&format!("t{t}")).expect("kv"), Some(PER_THREAD.to_le_bytes().to_vec()));
    }
    assert_eq!(s.last_read(&conv, Some(&ch)).expect("marker"), Some(hlc(PER_THREAD)));
}

#[test]
fn two_connections_to_one_file_wait_for_each_other() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("shared.db");
    let s1 = Arc::new(SqliteStorage::open(&path).expect("open 1"));
    let s2 = Arc::new(SqliteStorage::open(&path).expect("open 2"));
    let conv = ConvId([4; 32]);
    let writers: Vec<_> = [s1.clone(), s2.clone()]
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            std::thread::spawn(move || {
                let id = Identity::generate();
                for seq in 1..=100u64 {
                    let ev = msg(&id, conv, seq, hlc(i as u64 * 1_000 + seq), chan(1), "w");
                    assert!(s.insert_event(&ev).expect("insert under contention"));
                }
            })
        })
        .collect();
    for w in writers {
        w.join().expect("writer");
    }
    let heads = s1.heads(&conv).expect("heads");
    assert_eq!(heads.len(), 2);
    assert!(heads.iter().all(|(_, seq)| *seq == 100));
    assert_eq!(s2.heads(&conv).expect("heads"), heads);
    drop((s1, s2));
}

// ---- query plans and performance ----------------------------------------------------

/// `EXPLAIN QUERY PLAN` details for `sql` with `n` dummy parameters.
fn plan(s: &SqliteStorage, sql: &str, n: usize) -> String {
    let conn = s.conn();
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).expect("prepare");
    let args: Vec<i64> = vec![1; n];
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(3))
        .expect("plan")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("rows");
    rows.join("\n")
}

#[test]
fn hot_queries_use_indexes_without_sorting() {
    let s = mem();
    for (sql, n, index) in [
        (MSG_CHANNEL, 3, "events_channel_order"),
        (MSG_CHANNEL_BEFORE, 5, "events_channel_order"),
        (MSG_ALL, 2, "events_message_order"),
        (MSG_ALL_BEFORE, 4, "events_message_order"),
        (STATE_EVENTS, 1, "events_state"),
        (UNREAD_CHANNEL, 4, "events_channel_order"),
        (UNREAD_ALL, 3, "events_message_order"),
    ] {
        let p = plan(&s, sql, n);
        assert!(p.contains(index), "expected {index} in plan for {sql}:\n{p}");
        assert!(!p.contains("TEMP B-TREE"), "unexpected sort for {sql}:\n{p}");
        assert!(!p.contains("SCAN events"), "unexpected full scan for {sql}:\n{p}");
    }
    // The row-value cursor must be a range bound on the index, not a filter.
    let p = plan(&s, MSG_CHANNEL_BEFORE, 5);
    assert!(p.contains("(hlc,id)<(?,?)"), "cursor not used as index range:\n{p}");
}

#[test]
fn ten_thousand_messages_insert_and_page_quickly() {
    const N: u64 = 10_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let conv = ConvId([8; 32]);
    let ch = chan(1);
    let authors = [Identity::generate(), Identity::generate(), Identity::generate()];
    // Built and signed before any timing starts.
    let events: Vec<StoredEvent> = (0..N)
        .map(|i| {
            let author = &authors[(i % 3) as usize];
            let seq = i / 3 + 1;
            msg(author, conv, seq, hlc(i), ch, &format!("message number {i}, with a little bit of text in it"))
        })
        .collect();

    // Two fresh files; the best of the two must be under a second (other builds may be
    // competing for the CPU and disk).
    let mut best_insert = Duration::MAX;
    let mut store = None;
    for run in 0..2 {
        let s = SqliteStorage::open(dir.path().join(format!("perf{run}.db"))).expect("open");
        let t = Instant::now();
        assert_eq!(s.insert_events_batch(&events).expect("batch") as u64, N);
        best_insert = best_insert.min(t.elapsed());
        store = Some(s);
    }
    let s = store.expect("store");

    let cursor = Some(events[(N / 2) as usize].order_key());
    let expected: Vec<EventId> = events[..(N / 2) as usize].iter().rev().take(50).map(|e| e.id).collect();
    assert_eq!(ids(&s.messages(&conv, Some(&ch), cursor, 50).expect("warm")), expected);
    let mut best_page = Duration::MAX;
    for _ in 0..7 {
        let t = Instant::now();
        let page = s.messages(&conv, Some(&ch), cursor, 50).expect("page");
        best_page = best_page.min(t.elapsed());
        assert_eq!(page.len(), 50);
    }
    let t = Instant::now();
    let newest = s.messages(&conv, None, None, 50).expect("newest");
    let newest_time = t.elapsed();
    assert_eq!(newest[0].id, events[(N - 1) as usize].id);
    let t = Instant::now();
    let heads = s.heads(&conv).expect("heads");
    let heads_time = t.elapsed();
    assert_eq!(heads.len(), 3);

    eprintln!(
        "10k insert (best of 2): {best_insert:?}; 50-message page (best of 7): {best_page:?}; \
         newest page: {newest_time:?}; heads: {heads_time:?}"
    );
    assert!(best_insert < Duration::from_secs(1), "insert took {best_insert:?}");
    assert!(best_page < Duration::from_millis(10), "page took {best_page:?}");
    drop(s);
}
