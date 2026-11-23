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
