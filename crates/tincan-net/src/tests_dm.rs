//! DM follow-ups: end-of-sync marker, redial on a new address, display names, presence of
//! contacts who share no server (watch, keepalive, probe).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tincan_core::event::{Body, StoredEvent};
use tincan_core::keys::Identity;
use tincan_core::state::dm_channel;
use tincan_core::storage::MemStorage;
use tincan_core::sync::Heads;
use tincan_core::wire::Ctl;
use tincan_core::{ConvId, EventId, UserId, build};
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::NetEvent;
use crate::tests::{Node, fast, hint, node, start, until, within, wire};
use crate::timing::Timings;

async fn dm_say(n: &Node, conv: ConvId, text: &str) -> StoredEvent {
    let body = Body::Message { channel: EventId::default(), text: text.into(), attachments: vec![] };
    within("dm publish", Duration::from_secs(5), n.net.publish(conv, body)).await.expect("dm publish")
}

fn delivered(n: &Node, conv: &ConvId, id: &EventId) -> bool { n.net.with_state(conv, |s| s.message_authors.contains_key(id)).unwrap_or(false) }

/// Wait for `SyncComplete{conv}`, collecting the ids of `EventAdded` for `conv` before it.
async fn until_sync_complete(rx: &mut broadcast::Receiver<NetEvent>, conv: ConvId) -> HashSet<EventId> {
    let mut seen = HashSet::new();
    within("SyncComplete", Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(NetEvent::EventAdded { conv: c, event, .. }) if c == conv => {
                    seen.insert(event.id);
                }
                Ok(NetEvent::SyncComplete { conv: c }) if c == conv => return,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("event stream closed"),
            }
        }
    })
    .await;
    seen
}

fn sync_completes(rx: &mut broadcast::Receiver<NetEvent>, conv: ConvId) -> usize {
    let mut n = 0;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, NetEvent::SyncComplete { conv: c } if c == conv) {
            n += 1;
        }
    }
    n
}

/// Q1: a DM connection reports the end of the contact's history sync once: after the
/// messages it brought, or at once when there was nothing to bring.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dm_sync_complete_follows_the_contacts_history() {
    let a = node("alice", fast()).await;
    let (b_id, b_store) = (Identity::generate(), Arc::new(MemStorage::new()));
    let conv = a.net.dm_open(b_id.user()).await.expect("dm_open");
    let mut sent = Vec::new();
    for i in 0..5 {
        sent.push(dm_say(&a, conv, &format!("while you were away {i}")).await.id);
    }
    let b = start(b_id, b_store, "bob", fast()).await;
    let mut a_ev = a.net.subscribe();
    let mut b_ev = b.net.subscribe();
    a.net.add_addr_hint(b.net.me(), hint(&b));
    let seen = until_sync_complete(&mut b_ev, conv).await;
    assert!(sent.iter().all(|id| seen.contains(id)), "every message came before SyncComplete");
    // Bob had nothing Alice lacked: hers comes with the connection.
    until_sync_complete(&mut a_ev, conv).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(sync_completes(&mut a_ev, conv) + sync_completes(&mut b_ev, conv), 0, "once per connection");
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

/// Q2: a dial asked for while one is in flight goes out as soon as that dial fails when the
/// contact's address changed meanwhile (here: Bob restarted on a new port), instead of
/// after the retry backoff.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dm_redials_at_once_when_the_address_changes_mid_dial() {
    // Without the immediate redial, delivery would wait for the 60 s retry.
    let t = Timings { dm_retry: vec![Duration::from_secs(60)], ..fast() };
    let a = node("alice", t.clone()).await;
    let (b_id, b_store) = (Identity::generate(), Arc::new(MemStorage::new()));
    let b_old = start(b_id.clone(), b_store.clone(), "bob", t.clone()).await;
    let old = hint(&b_old);
    b_old.net.shutdown().await;
    drop(b_old);
    a.net.add_addr_hint(b_id.user(), old);
    // The dial to Bob's old address is out now (it can only time out).
    let conv = a.net.dm_open(b_id.user()).await.expect("dm_open");
    let m = dm_say(&a, conv, "are you there?").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let b = start(b_id, b_store, "bob", t).await;
    let t0 = Instant::now();
    a.net.add_addr_hint(b.net.me(), hint(&b));
    until("delivered to the restarted contact", Duration::from_secs(10), || delivered(&b, &conv, &m.id)).await;
    let took = t0.elapsed();
    eprintln!("delivered {took:?} after the new address arrived");
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

fn name_changes(rx: &mut broadcast::Receiver<NetEvent>, peer: UserId) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let NetEvent::DmNameChanged { peer: p, name, .. } = ev
            && p == peer
        {
            out.push(name);
        }
    }
    out
}

/// Q5: both sides announce their display name on the DM connection and again when it
/// changes; invalid names are ignored; the name is kept across a restart. A stranger's name
/// is stored once their first message opens the conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dm_display_names_are_announced_checked_and_kept() {
    let a = node("alice", fast()).await;
    let (b_id, b_store) = (Identity::generate(), Arc::new(MemStorage::new()));
    let b = start(b_id.clone(), b_store.clone(), "bob", fast()).await;
    wire(&[&a, &b]);
    let mut b_ev = b.net.subscribe();
    let conv = a.net.dm_open(b.net.me()).await.expect("dm_open");
    assert_eq!(a.net.dm_peer_name(&conv), None);
    dm_say(&a, conv, "hi").await;
    until("bob knows alice's name", Duration::from_secs(5), || b.net.dm_peer_name(&conv).as_deref() == Some("alice")).await;
    until("alice knows bob's name", Duration::from_secs(5), || a.net.dm_peer_name(&conv).as_deref() == Some("bob")).await;
    // A rename reaches the open connection (trimmed).
    a.net.set_display_name("  Alice Cooper ");
    until("rename arrives", Duration::from_secs(5), || b.net.dm_peer_name(&conv).as_deref() == Some("Alice Cooper")).await;
    // Invalid names change nothing: control characters, too long, blank.
    let too_long = "x".repeat(tincan_core::state::MAX_NAME_CHARS + 1);
    for bad in ["bad\u{7}name", too_long.as_str(), "   "] {
        assert!(a.net.send_dm_ctl(&b.net.me(), &Ctl::DmProfile { name: bad.to_string() }));
    }
    assert!(a.net.send_dm_ctl(&b.net.me(), &Ctl::DmProfile { name: "Al".into() }));
    until("the valid name after them arrives", Duration::from_secs(5), || b.net.dm_peer_name(&conv).as_deref() == Some("Al")).await;
    assert_eq!(name_changes(&mut b_ev, a.net.me()), vec!["alice".to_string(), "Alice Cooper".to_string(), "Al".to_string()]);
    // Kept across a restart, before any connection.
    b.net.shutdown().await;
    drop(b);
    let b = start(b_id, b_store, "bob", fast()).await;
    assert_eq!(b.net.dm_peer_name(&conv).as_deref(), Some("Al"));

    // A stranger announces a name first, then opens the conversation with a message.
    let x = Identity::generate();
    let ep = crate::build_endpoint(&x, &crate::tests::cfg("x")).await.expect("endpoint");
    let xconv = ConvId::dm(&a.net.me(), &x.user());
    let conn = crate::link::connect(&ep, &a.net.me(), Some(&hint(&a)), tincan_core::ALPN_DM, Duration::from_secs(3)).await.expect("connect");
    let mut d = crate::link::exchange(conn, &Ctl::DmHello { conv: xconv, heads: Heads::default(), have: vec![] }, Duration::from_secs(3))
        .await
        .expect("hello");
    crate::link::write_ctl(&mut d.send, &Ctl::DmProfile { name: "xavier".into() }).await.expect("profile");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(a.net.conv_state(&xconv).is_none(), "a name alone opens nothing");
    let first = build::event(&x, xconv, 1, tincan_core::hlc::Hlc::new().now(), Body::Message { channel: dm_channel(&xconv), text: "hello".into(), attachments: vec![] })
        .expect("event");
    crate::link::write_ctl(&mut d.send, &Ctl::Events { events: vec![first], more: false }).await.expect("events");
    until("stranger's name stored", Duration::from_secs(5), || a.net.dm_peer_name(&xconv).as_deref() == Some("xavier")).await;
    drop(d);
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

/// The app keeps watches in shared state and awaits probes on Tauri's runtime.
#[test]
fn dm_watch_and_probe_are_send() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<crate::DmWatch>();
    #[allow(dead_code)]
    fn probe_is_send(n: &crate::Net) -> impl std::future::Future<Output = bool> + Send + '_ { n.dm_reachable(UserId([0; 32])) }
}

/// Short DM idle and keepalive, quick watch redials.
fn presence_timings() -> Timings {
    Timings {
        dm_idle: Duration::from_millis(800),
        dm_keepalive: Duration::from_millis(250),
        dm_watch_retry: vec![Duration::from_millis(200), Duration::from_millis(400)],
        ..fast()
    }
}

/// SPEC-GAPS-2: a watch on one side keeps a DM-only contact connected (online) through many
/// idle periods: the contact's side would otherwise close the connection as idle. Once the
/// watch is dropped, the connection idles out as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dm_watch_keeps_a_contact_online_past_the_idle_timeout() {
    let a = node("alice", presence_timings()).await;
    let b = node("bob", presence_timings()).await;
    wire(&[&a, &b]);
    let conv = a.net.dm_open(b.net.me()).await.expect("dm_open");
    let m = dm_say(&a, conv, "hi").await;
    until("delivered", Duration::from_secs(5), || delivered(&b, &conv, &m.id) && a.net.dm_pending(&conv) == 0).await;
    // Without a watch the idle connection closes.
    until("idle close", Duration::from_secs(5), || !a.net.peer_online(&b.net.me())).await;
    // A one-sided hold does not keep it either: the contact's side closes it.
    let hold = a.net.dm_hold(b.net.me());
    a.net.dm_connect(b.net.me()).await.expect("connect");
    until("closed by the contact despite the hold", Duration::from_secs(5), || !a.net.peer_online(&b.net.me())).await;
    drop(hold);
    // A watch does.
    let watch = a.net.dm_watch(b.net.me());
    until("watch connects", Duration::from_secs(5), || a.net.peer_online(&b.net.me()) && b.net.peer_online(&a.net.me())).await;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(3000) {
        assert!(a.net.peer_online(&b.net.me()) && b.net.peer_online(&a.net.me()), "dropped after {:?}", t0.elapsed());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(watch);
    until("idle close after the watch ends", Duration::from_secs(5), || !a.net.peer_online(&b.net.me()) && !b.net.peer_online(&a.net.me())).await;
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

/// SPEC-GAPS-2: a watched contact who restarts comes back online without anyone sending a
/// message (the watch redials, and a new address for them is used at once).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dm_watch_recovers_presence_after_the_contact_restarts() {
    let a = node("alice", presence_timings()).await;
    let (b_id, b_store) = (Identity::generate(), Arc::new(MemStorage::new()));
    let b = start(b_id.clone(), b_store.clone(), "bob", presence_timings()).await;
    wire(&[&a, &b]);
    let conv = a.net.dm_open(b.net.me()).await.expect("dm_open");
    let m = dm_say(&a, conv, "hi").await;
    until("delivered", Duration::from_secs(5), || delivered(&b, &conv, &m.id) && a.net.dm_pending(&conv) == 0).await;
    let _watch = a.net.dm_watch(b.net.me());
    until("online", Duration::from_secs(5), || a.net.peer_online(&b.net.me())).await;
    b.net.shutdown().await;
    drop(b);
    until("offline once they quit", Duration::from_secs(15), || !a.net.peer_online(&b_id.user())).await;
    let b = start(b_id, b_store, "bob", presence_timings()).await;
    // Their new address (relay or LAN discovery outside tests).
    a.net.add_addr_hint(b.net.me(), hint(&b));
    until("online again, no message sent", Duration::from_secs(10), || a.net.peer_online(&b.net.me())).await;
    assert_eq!(a.net.dm_pending(&conv), 0);
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

/// SPEC-GAPS-2: `dm_reachable` dials a contact whose idle connection closed; after a failed
/// probe the next one answers at once; users without a DM conversation are never dialed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dm_reachable_probes_with_a_backoff() {
    let t = Timings { dm_watch_retry: vec![Duration::from_secs(30)], ..presence_timings() };
    let a = node("alice", t.clone()).await;
    let b = node("bob", t).await;
    wire(&[&a, &b]);
    let conv = a.net.dm_open(b.net.me()).await.expect("dm_open");
    let m = dm_say(&a, conv, "hi").await;
    until("delivered", Duration::from_secs(5), || delivered(&b, &conv, &m.id) && a.net.dm_pending(&conv) == 0).await;
    until("idle close", Duration::from_secs(5), || !a.net.peer_online(&b.net.me())).await;
    assert!(within("probe", Duration::from_secs(10), a.net.dm_reachable(b.net.me())).await, "reachable");
    assert!(a.net.peer_online(&b.net.me()), "the probe's connection is up");
    let stranger = Identity::generate().user();
    let t0 = Instant::now();
    assert!(!a.net.dm_reachable(stranger).await);
    assert!(t0.elapsed() < Duration::from_millis(100), "no dial for a user without a DM");
    assert!(a.net.conv_state(&ConvId::dm(&a.net.me(), &stranger)).is_none(), "no conversation opened");
    let bid = b.net.me();
    b.net.shutdown().await;
    until("offline", Duration::from_secs(15), || !a.net.peer_online(&bid)).await;
    assert!(!within("failed probe", Duration::from_secs(10), a.net.dm_reachable(bid)).await);
    let t0 = Instant::now();
    assert!(!a.net.dm_reachable(bid).await);
    assert!(t0.elapsed() < Duration::from_millis(100), "a probe right after a failed one answers at once");
    a.net.shutdown().await;
}
