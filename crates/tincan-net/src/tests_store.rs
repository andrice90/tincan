//! History sync into the real SQLite storage (tincan-store, an on-disk file in WAL mode)
//! instead of `MemStorage`.

use std::sync::Arc;
use std::time::Duration;

use tincan_core::event::Body;
use tincan_core::keys::Identity;
use tincan_core::storage::Storage;
use tincan_core::UserId;
use tincan_store::SqliteStorage;
use tokio::time::Instant;

use crate::tests::{cfg, fast, hint, node, text_channel, until, within};
use crate::{Net, build_endpoint};

/// Events in the history the joiners sync.
const HISTORY: usize = 1000;

async fn start_sqlite(identity: Identity, storage: Arc<SqliteStorage>, name: &str) -> Net {
    let c = cfg(name);
    let ep = build_endpoint(&identity, &c).await.expect("endpoint");
    Net::start_with(ep, storage, identity, c, |b| b, fast()).await.expect("start")
}

/// Joiners whose storage is an SQLite file sync a 1000-message history: every event is
/// stored. Prints how long each sync took (from `join` until the last event is in the file).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_sync_into_sqlite() {
    let a = node("alice", fast()).await;
    let server = a.net.create_server("Big").await.expect("create");
    let ch = text_channel(&a, &server);
    for i in 0..HISTORY {
        let body = Body::Message { channel: ch, text: format!("message number {i} with a little text in it"), attachments: vec![] };
        within("publish", Duration::from_secs(10), a.net.publish(server, body)).await.expect("publish");
    }
    let want: Vec<(UserId, u64)> = a.storage.heads(&server).expect("heads");
    let invite = a.net.create_invite(server).await.expect("invite");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut took = Vec::new();
    for round in 0..3 {
        let store = Arc::new(SqliteStorage::open(dir.path().join(format!("joiner{round}.db"))).expect("sqlite"));
        let b = start_sqlite(Identity::generate(), store.clone(), "bob").await;
        b.add_addr_hint(a.net.me(), hint(&a));
        let t0 = Instant::now();
        within("join", Duration::from_secs(20), b.join(&invite)).await.expect("join");
        until("history stored in sqlite", Duration::from_secs(60), || {
            let heads = store.heads(&server).unwrap_or_default();
            want.iter().all(|(u, s)| heads.iter().any(|(hu, hs)| hu == u && hs >= s))
        })
        .await;
        took.push(t0.elapsed());
        let n: usize = want.iter().map(|(u, s)| store.events_range(&server, u, 1, *s).map(|v| v.len()).unwrap_or(0)).sum();
        assert!(n > HISTORY, "every event stored ({n})");
        b.shutdown().await;
    }
    eprintln!("sync of {HISTORY} events into sqlite: {took:?}");
    a.net.shutdown().await;
}

/// NET-SECURITY-9: the owner's chain blocks on a later member's invite with more events after
/// it than the joiner's pending buffer holds per author. The joiner reports SyncComplete only
/// once every event is applied (what it dropped meanwhile is asked for again first).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_complete_waits_for_events_dropped_while_blocked() {
    use crate::tests::wire;
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let server = crate::tests::cluster(&a, &[&b]).await;
    let bid = b.net.me();
    within("bob's invite", Duration::from_secs(10), b.net.create_invite(server)).await.expect("invite");
    let mut bob_invite = None;
    until("alice knows bob's invite", Duration::from_secs(5), || {
        bob_invite = a.net.conv_state(&server).and_then(|s| s.invites.values().find(|i| i.creator == bid).map(|i| i.invite_pk));
        bob_invite.is_some()
    })
    .await;
    // Alice (the owner) admits someone through Bob's invite: her chain now waits on Bob's.
    let body = Body::MemberAdmit { user: Identity::generate().user(), invite_pk: bob_invite.unwrap(), from_seq: 1 };
    within("admit", Duration::from_secs(10), a.net.publish(server, body)).await.expect("admit");
    let ch = text_channel(&a, &server);
    for i in 0..2200 {
        let body = Body::Message { channel: ch, text: format!("after the admission {i}"), attachments: vec![] };
        within("publish", Duration::from_secs(10), a.net.publish(server, body)).await.expect("publish");
    }
    let invite = a.net.create_invite(server).await.expect("invite");
    let want = a.net.conv_state(&server).unwrap().sync_heads();
    let j = node("joy", fast()).await;
    wire(&[&a, &b, &j]);
    let mut j_ev = j.net.subscribe();
    within("join", Duration::from_secs(20), j.net.join(&invite)).await.expect("join");
    within("SyncComplete", Duration::from_secs(60), async {
        loop {
            match j_ev.recv().await {
                Ok(crate::NetEvent::SyncComplete { conv }) if conv == server => return,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("{e}"),
            }
        }
    })
    .await;
    let got = j.net.conv_state(&server).unwrap();
    for (u, s) in want.0 {
        assert!(got.head(&u) >= s, "at SyncComplete {} has {} of {s}", u.short(), got.head(&u));
    }
    for n in [a, b, j] {
        n.net.shutdown().await;
    }
}
