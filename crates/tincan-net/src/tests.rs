//! In-process multi-endpoint tests: relays disabled, `presets::Minimal`, loopback sockets,
//! peers wired with `add_addr_hint` (no global discovery).

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tincan_core::event::{Body, ChannelKind};
use tincan_core::invite::Invite;
use tincan_core::keys::Identity;
use tincan_core::storage::{MemStorage, Storage};
use tincan_core::wire::{AddrHint, Ctl, RejectCode};
use tincan_core::{BlobHash, ChannelId, ConvId, EventId, ServerId, UserId, build, now_ms};
use tokio::time::Instant;

use crate::timing::Timings;
use crate::{ConnState, Net, NetConfig, NetError, NetEvent, RelayChoice, build_endpoint};

/// Timings shrunk for tests (liveness kept realistic unless a test overrides it).
pub(super) fn fast() -> Timings {
    Timings {
        tick: Duration::from_millis(500),
        host_suspect: Duration::from_millis(1500),
        host_dead: Duration::from_millis(2500),
        stagger: Duration::from_millis(1500),
        dial: Duration::from_secs(2),
        dial_stagger: Duration::from_millis(20),
        solo_after: Duration::from_millis(1500),
        solo_retry: Duration::from_secs(3),
        host_probe: Duration::from_secs(3),
        roster_debounce: Duration::from_millis(50),
        merge_timeout: Duration::from_secs(3),
        failover_redial: Duration::from_millis(300),
        dm_retry: vec![Duration::from_millis(500), Duration::from_secs(1)],
        compare_cooldown: Duration::from_secs(1),
        roster_min_interval: Duration::from_millis(200),
        publish_park_retry: Duration::from_millis(500),
        ..Timings::default()
    }
}

pub(super) struct Node {
    pub(super) net: Net,
    pub(super) storage: Arc<MemStorage>,
    pub(super) identity: Identity,
}

pub(super) fn cfg(name: &str) -> NetConfig {
    NetConfig {
        relay: RelayChoice::Disabled,
        mdns: false,
        bind_addr: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)),
        display_name: name.to_string(),
        ..NetConfig::default()
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

pub(super) async fn start(identity: Identity, storage: Arc<MemStorage>, name: &str, timings: Timings) -> Node {
    init_tracing();
    let c = cfg(name);
    // Binding can fail transiently when many tests bind loopback sockets at once.
    let mut attempt = 0;
    let ep = loop {
        match build_endpoint(&identity, &c).await {
            Ok(ep) => break ep,
            Err(e) if attempt < 3 => {
                eprintln!("binding the endpoint failed ({e}); retrying");
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("endpoint: {e}"),
        }
    };
    let net = Net::start_with(ep, storage.clone(), identity.clone(), c, |b| b, timings).await.expect("start");
    Node { net, storage, identity }
}

pub(super) async fn node(name: &str, timings: Timings) -> Node { start(Identity::generate(), Arc::new(MemStorage::new()), name, timings).await }

/// Loopback address of a node (IPv4 socket only).
pub(super) fn hint(n: &Node) -> AddrHint {
    let direct = n
        .net
        .endpoint()
        .bound_sockets()
        .into_iter()
        .filter(|s| s.is_ipv4())
        .map(|s| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), s.port()))
        .collect();
    AddrHint { relay: None, direct }
}

pub(super) fn wire(nodes: &[&Node]) {
    for a in nodes {
        for b in nodes {
            if a.net.me() != b.net.me() {
                a.net.add_addr_hint(b.net.me(), hint(b));
            }
        }
    }
}

/// Only `others` learn `host`'s address (as from an invite); everything else must come from
/// the protocol (roster addresses).
pub(super) fn wire_to(host: &Node, others: &[&Node]) {
    for o in others {
        o.net.add_addr_hint(host.net.me(), hint(host));
    }
}

pub(super) async fn until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        if Instant::now() > deadline {
            panic!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(super) async fn within<T>(what: &str, timeout: Duration, fut: impl Future<Output = T>) -> T {
    match tokio::time::timeout(timeout, fut).await {
        Ok(v) => v,
        Err(_) => panic!("timed out: {what}"),
    }
}

pub(super) fn text_channel(n: &Node, server: &ServerId) -> ChannelId {
    let st = n.net.conv_state(server).expect("state");
    st.sorted_channels().iter().find(|c| c.kind == ChannelKind::Text).expect("text channel").id
}

pub(super) fn voice_channel(n: &Node, server: &ServerId) -> ChannelId {
    let st = n.net.conv_state(server).expect("state");
    st.sorted_channels().iter().find(|c| c.kind == ChannelKind::Voice).expect("voice channel").id
}

async fn say(n: &Node, server: ServerId, channel: ChannelId, text: &str) -> EventId {
    let body = Body::Message { channel, text: text.to_string(), attachments: vec![] };
    within("publish", Duration::from_secs(10), n.net.publish(server, body)).await.expect("publish").id
}

fn has_event(n: &Node, conv: &ConvId, id: &EventId) -> bool {
    n.storage.event(id).ok().flatten().is_some() && n.net.conv_state(conv).is_some_and(|s| s.message_authors.contains_key(id))
}

pub(super) fn host_of(n: &Node, server: &ServerId) -> Option<UserId> {
    let c = n.net.connection(server);
    match c.state {
        ConnState::Member | ConnState::Hosting | ConnState::Solo => c.host,
        _ => None,
    }
}

/// Server hosted by `a`, joined by the others. Waits until everyone is connected.
pub(super) async fn cluster(a: &Node, others: &[&Node]) -> ServerId {
    let server = a.net.create_server("Test server").await.expect("create");
    let invite = a.net.create_invite(server).await.expect("invite");
    for o in others {
        let joined = within("join", Duration::from_secs(20), o.net.join(&invite)).await.expect("join");
        assert_eq!(joined, server);
    }
    let me = a.net.me();
    until("everyone connected to a", Duration::from_secs(10), || others.iter().all(|o| host_of(o, &server) == Some(me))).await;
    until("roster complete", Duration::from_secs(10), || {
        a.net.roster(&server).is_some_and(|r| r.online.len() == others.len() + 1)
    })
    .await;
    server
}

pub(super) async fn wait_succession(a: &Node, server: &ServerId, n: usize) -> Vec<UserId> {
    until("succession filled", Duration::from_secs(10), || a.net.roster(server).is_some_and(|r| r.succession.len() >= n)).await;
    a.net.roster(server).map(|r| r.succession).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_invite_join_sync_three_members() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire(&[&a, &b, &c]);
    let server = a.net.create_server("Test server").await.unwrap();
    assert_eq!(a.net.connection(&server).state, ConnState::Solo);
    let ch = text_channel(&a, &server);
    let early = say(&a, server, ch, "before anyone joined").await;
    let invite = a.net.create_invite(server).await.unwrap();
    assert!(invite.starts_with("tincan1"));
    let mut b_events = b.net.subscribe();
    assert_eq!(b.net.join(&invite).await.unwrap(), server);
    assert_eq!(c.net.join(&invite).await.unwrap(), server);
    // Joining again is a no-op.
    assert_eq!(b.net.join(&invite).await.unwrap(), server);
    until("members connected", Duration::from_secs(10), || {
        host_of(&b, &server) == Some(a.net.me()) && host_of(&c, &server) == Some(a.net.me())
    })
    .await;
    until("history synced to joiners", Duration::from_secs(10), || has_event(&b, &server, &early) && has_event(&c, &server, &early)).await;
    for n in [&a, &b, &c] {
        let st = n.net.conv_state(&server).unwrap();
        for m in [&a, &b, &c] {
            assert!(st.is_member(&m.net.me()), "membership");
        }
        assert_eq!(st.name, "Test server");
    }
    assert_eq!(b.net.conv_state(&server).unwrap().display_name(&c.net.me()), "carol");
    // Live fan-out both ways.
    let from_b = say(&b, server, ch, "hello from bob").await;
    let from_c = say(&c, server, ch, "hello from carol").await;
    until("fan-out", Duration::from_secs(10), || {
        [&a, &b, &c].iter().all(|n| has_event(n, &server, &from_b) && has_event(n, &server, &from_c))
    })
    .await;
    // Events reached the app.
    let mut saw = false;
    while let Ok(ev) = b_events.try_recv() {
        if matches!(ev, NetEvent::EventAdded { event, .. } if event.id == from_c) {
            saw = true;
        }
    }
    assert!(saw, "EventAdded for carol's message");
    // Roster and presence.
    until("roster", Duration::from_secs(5), || b.net.roster(&server).is_some_and(|r| r.online.len() == 3)).await;
    assert!(b.net.peer_online(&c.net.me()));
    // Typing is forwarded.
    let mut c_events = c.net.subscribe();
    b.net.typing(server, ch).await;
    let typed = within("typing", Duration::from_secs(5), async {
        loop {
            if let Ok(NetEvent::Typing { user, .. }) = c_events.recv().await {
                return user;
            }
        }
    })
    .await;
    assert_eq!(typed, b.net.me());
    // Holders through the host.
    let blob = BlobHash([7; 32]);
    c.net.have_add(blob);
    let mut holders = Vec::new();
    for _ in 0..40 {
        holders = b.net.who_has(&server, blob).await;
        if !holders.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(holders.iter().map(|(u, _)| *u).collect::<Vec<_>>(), vec![c.net.me()]);
    // Short ids are distinct.
    let r = a.net.roster(&server).unwrap();
    let mut ids: Vec<u16> = r.online.iter().map(|e| e.short_id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3);
    assert_eq!(b.net.my_short_id(&server), r.online.iter().find(|e| e.user == b.net.me()).map(|e| e.short_id));
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_fan_out_keeps_author_order() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire(&[&a, &b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let ch = text_channel(&a, &server);
    let mut ids = Vec::new();
    for i in 0..20 {
        ids.push(say(&b, server, ch, &format!("msg {i}")).await);
    }
    until("all 20 at carol", Duration::from_secs(10), || ids.iter().all(|id| has_event(&c, &server, id))).await;
    // Carol stores bob's chain contiguously and in order.
    let head = c.storage.heads(&server).unwrap().into_iter().find(|(u, _)| *u == b.net.me()).unwrap().1;
    let chain = c.storage.events_range(&server, &b.net.me(), 1, head).unwrap();
    let seqs: Vec<u64> = chain.iter().map(|e| e.event.seq).collect();
    assert_eq!(seqs, (1..=head).collect::<Vec<_>>());
    let page = c.storage.messages(&server, Some(&ch), None, 100).unwrap();
    let mut texts: Vec<String> = page
        .iter()
        .rev()
        .filter(|e| e.event.author == b.net.me())
        .filter_map(|e| match &e.event.body {
            Body::Message { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    let expect: Vec<String> = (0..20).map(|i| format!("msg {i}")).collect();
    assert_eq!(texts.len(), 20);
    texts.truncate(20);
    assert_eq!(texts, expect, "display order = author order");
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clean_handoff_moves_host_without_loss() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire_to(&a, &[&b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let ch = text_channel(&a, &server);
    let succ = wait_succession(&a, &server, 2).await;
    let s1 = succ[0];
    let mut sent = Vec::new();
    for (i, n) in [&a, &b, &c].iter().enumerate() {
        sent.push(say(n, server, ch, &format!("before handoff {i}")).await);
    }
    let (new_host, other) = if s1 == b.net.me() { (&b, &c) } else { (&c, &b) };
    let ids_before = (new_host.net.my_short_id(&server), other.net.my_short_id(&server));
    let t0 = Instant::now();
    let leaving = a.net.clone();
    let shutdown = tokio::spawn(async move { leaving.shutdown().await });
    until("S1 hosts and the other member follows", Duration::from_secs(5), || {
        host_of(new_host, &server) == Some(s1) && host_of(other, &server) == Some(s1)
    })
    .await;
    let took = t0.elapsed();
    eprintln!("clean handoff: S1 hosting and the other member moved after {took:?}");
    assert!(took < Duration::from_secs(2), "handoff took {took:?}");
    shutdown.await.expect("shutdown task");
    assert_eq!(new_host.net.connection(&server).state, ConnState::Hosting);
    until("short ids kept", Duration::from_secs(5), || {
        (new_host.net.my_short_id(&server), other.net.my_short_id(&server)) == ids_before
    })
    .await;
    for id in &sent {
        assert!(has_event(new_host, &server, id) && has_event(other, &server, id), "no message loss");
    }
    let after = say(other, server, ch, "after handoff").await;
    until("new host fans out", Duration::from_secs(5), || has_event(new_host, &server, &after)).await;
    assert!(new_host.net.connection(&server).epoch >= 2);
    for n in [b, c] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_crash_fails_over_to_successor() {
    // Realistic liveness (tick 2 s, dead at 10 s); everything else fast.
    let t = Timings { tick: Duration::from_secs(2), host_suspect: Duration::from_secs(6), host_dead: Duration::from_secs(10), ..fast() };
    let a = node("alice", t.clone()).await;
    let b = node("bob", t.clone()).await;
    let c = node("carol", t.clone()).await;
    wire_to(&a, &[&b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let ch = text_channel(&a, &server);
    let succ = wait_succession(&a, &server, 2).await;
    let s1 = succ[0];
    until("members know the succession", Duration::from_secs(5), || {
        [&b, &c].iter().all(|n| n.net.roster(&server).is_some_and(|r| r.succession.first() == Some(&s1)))
    })
    .await;
    let before = say(&b, server, ch, "before crash").await;
    until("message everywhere", Duration::from_secs(5), || has_event(&c, &server, &before)).await;
    let ids_before = (b.net.my_short_id(&server), c.net.my_short_id(&server));
    let t0 = Instant::now();
    a.net.crash();
    until("failover to S1", Duration::from_secs(20), || host_of(&b, &server) == Some(s1) && host_of(&c, &server) == Some(s1)).await;
    let took = t0.elapsed();
    eprintln!("crash failover: both members on S1 after {took:?}");
    assert!(took > Duration::from_secs(9), "detection must come from tick silence, took {took:?}");
    assert!(took < Duration::from_secs(15), "failover took {took:?}");
    let (new_host, other) = if s1 == b.net.me() { (&b, &c) } else { (&c, &b) };
    until("short ids kept", Duration::from_secs(5), || (b.net.my_short_id(&server), c.net.my_short_id(&server)) == ids_before).await;
    let after = say(other, server, ch, "after crash").await;
    until("new host has it", Duration::from_secs(5), || has_event(new_host, &server, &after)).await;
    for n in [b, c] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_solo_hosts_merge_with_union_history() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let server = cluster(&a, &[&b]).await;
    let ch = text_channel(&a, &server);
    let shared = say(&a, server, ch, "shared").await;
    until("b has shared", Duration::from_secs(5), || has_event(&b, &server, &shared)).await;
    // Partition: restart both on fresh ports with no addresses for each other.
    let (a_id, a_store) = (a.identity.clone(), a.storage.clone());
    let (b_id, b_store) = (b.identity.clone(), b.storage.clone());
    a.net.shutdown().await;
    b.net.shutdown().await;
    let a = start(a_id, a_store, "alice", fast()).await;
    let b = start(b_id, b_store, "bob", fast()).await;
    until("both solo", Duration::from_secs(10), || {
        a.net.connection(&server).state == ConnState::Solo && b.net.connection(&server).state == ConnState::Solo
    })
    .await;
    let only_a = say(&a, server, ch, "written by a while split").await;
    let only_b = say(&b, server, ch, "written by b while split").await;
    // Heal the partition.
    wire(&[&a, &b]);
    until("one host again", Duration::from_secs(15), || {
        let ha = host_of(&a, &server);
        ha.is_some() && ha == host_of(&b, &server) && {
            let states = [a.net.connection(&server).state, b.net.connection(&server).state];
            states.contains(&ConnState::Member) && states.contains(&ConnState::Hosting)
        }
    })
    .await;
    until("union history", Duration::from_secs(10), || {
        [&a, &b].iter().all(|n| [&shared, &only_a, &only_b].iter().all(|id| has_event(n, &server, id)))
    })
    .await;
    let epoch = a.net.connection(&server).epoch;
    assert!(epoch >= 3, "merge bumps the epoch above both solo epochs (got {epoch})");
    // Both sides can publish after the merge (the loser is a normal member).
    let after_a = say(&a, server, ch, "after merge from a").await;
    let after_b = say(&b, server, ch, "after merge from b").await;
    until("post-merge fan-out", Duration::from_secs(5), || {
        [&a, &b].iter().all(|n| has_event(n, &server, &after_a) && has_event(n, &server, &after_b))
    })
    .await;
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ephemeral_state_restored_after_migration() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire_to(&a, &[&b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let voice = voice_channel(&a, &server);
    let succ = wait_succession(&a, &server, 2).await;
    let s1 = succ[0];
    // The non-successor member carries the ephemeral state, so it must survive the move.
    let (holder, asker) = if s1 == b.net.me() { (&c, &b) } else { (&b, &c) };
    // The roster's room comes from what the host's SFU forwards (VoiceJoin), mute from Status.
    let _session = within("voice_join", Duration::from_secs(5), holder.net.voice_join(server, voice)).await.expect("voice_join");
    holder.net.set_status(|s| s.muted = true);
    let blob = BlobHash([9; 32]);
    holder.net.have_add(blob);
    until("host sees voice", Duration::from_secs(5), || {
        a.net.roster(&server).is_some_and(|r| r.online.iter().any(|e| e.user == holder.net.me() && e.voice == Some(voice) && e.muted))
    })
    .await;
    let holder_id = holder.net.my_short_id(&server);
    a.net.shutdown().await;
    until("moved", Duration::from_secs(5), || host_of(holder, &server) == Some(s1) && host_of(asker, &server) == Some(s1)).await;
    until("voice state restored on the new host", Duration::from_secs(5), || {
        asker
            .net
            .roster(&server)
            .is_some_and(|r| r.host == s1 && r.online.iter().any(|e| e.user == holder.net.me() && e.voice == Some(voice) && e.muted))
    })
    .await;
    let mut holders = Vec::new();
    for _ in 0..40 {
        holders = asker.net.who_has(&server, blob).await;
        if !holders.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(holders.iter().any(|(u, _)| *u == holder.net.me()), "holder index rebuilt: {holders:?}");
    // Short ids survive the migration.
    assert!(holder_id.is_some());
    until("holder keeps its short id", Duration::from_secs(5), || holder.net.my_short_id(&server) == holder_id).await;
    let new_roster = asker.net.roster(&server).unwrap();
    assert_eq!(new_roster.online.iter().find(|e| e.user == holder.net.me()).map(|e| e.short_id), holder_id);
    for n in [b, c] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn member_restart_reconnects_via_cache() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let server = cluster(&a, &[&b]).await;
    let ch = text_channel(&a, &server);
    let (b_id, b_store) = (b.identity.clone(), b.storage.clone());
    b.net.shutdown().await;
    let missed = say(&a, server, ch, "while bob was away").await;
    let b = start(b_id, b_store, "bob", fast()).await;
    // Bob's cache holds alice's address from the roster; no new hint needed.
    until("bob reconnects", Duration::from_secs(10), || host_of(&b, &server) == Some(a.net.me())).await;
    until("bob catches up", Duration::from_secs(5), || has_event(&b, &server, &missed)).await;
    let back = say(&b, server, ch, "back").await;
    until("alice has it", Duration::from_secs(5), || has_event(&a, &server, &back)).await;
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dm_sent_offline_is_delivered_on_reconnect() {
    let a = node("alice", fast()).await;
    let b_id = Identity::generate();
    let b_store = Arc::new(MemStorage::new());
    let conv = a.net.dm_open(b_id.user()).await.unwrap();
    assert_eq!(conv, ConvId::dm(&a.net.me(), &b_id.user()));
    let mut a_events = a.net.subscribe();
    let m1 = a.net.publish(conv, Body::Message { channel: EventId::default(), text: "are you there?".into(), attachments: vec![] }).await.unwrap();
    let m2 = a.net.publish(conv, Body::Message { channel: EventId::default(), text: "ping".into(), attachments: vec![] }).await.unwrap();
    assert_eq!(a.net.dm_pending(&conv), m2.event.seq);
    assert_eq!(a.net.dm_delivered(&conv), 0);
    // Bob comes online later.
    tokio::time::sleep(Duration::from_millis(600)).await;
    let b = start(b_id, b_store, "bob", fast()).await;
    a.net.add_addr_hint(b.net.me(), hint(&b));
    until("delivered to bob", Duration::from_secs(10), || {
        b.net.conv_state(&conv).is_some_and(|s| s.message_authors.contains_key(&m1.id) && s.message_authors.contains_key(&m2.id))
    })
    .await;
    until("ack", Duration::from_secs(5), || a.net.dm_pending(&conv) == 0).await;
    assert_eq!(a.net.dm_delivered(&conv), m2.event.seq);
    let mut delivered = false;
    while let Ok(ev) = a_events.try_recv() {
        if matches!(ev, NetEvent::DmDelivered { up_to_seq, .. } if up_to_seq == m2.event.seq) {
            delivered = true;
        }
    }
    assert!(delivered, "DmDelivered emitted");
    // Reply over the open connection.
    let reply = b.net.publish(conv, Body::Message { channel: EventId::default(), text: "yes".into(), attachments: vec![] }).await.unwrap();
    until("reply at alice", Duration::from_secs(5), || a.net.conv_state(&conv).is_some_and(|s| s.message_authors.contains_key(&reply.id))).await;
    assert!(a.net.peer_online(&b.net.me()));
    // Server-only bodies are refused in DMs.
    let bad = a.net.publish(conv, Body::Profile { name: "x".into() }).await;
    assert!(matches!(bad, Err(NetError::Invalid(_))));
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invite_rejections() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let server = a.net.create_server("Guarded").await.unwrap();
    let code = a.net.create_invite(server).await.unwrap();
    let good = Invite::decode(&code).unwrap();

    // Garbage and user codes.
    assert!(matches!(b.net.join("not an invite").await, Err(NetError::Invalid(_))));
    let user_code = tincan_core::invite::user_code(&a.net.me());
    assert!(matches!(b.net.join(&user_code).await, Err(NetError::Invalid(_))));

    // Expired locally.
    let mut expired = good.clone();
    expired.expires_ms = now_ms() - 1;
    match b.net.join(&expired.encode()).await {
        Err(NetError::Rejected(m)) => assert!(m.contains("expired"), "{m}"),
        other => panic!("expected expiry, got {other:?}"),
    }

    // Wrong invite key (bad signature / unknown invite).
    let mut forged = good.clone();
    forged.invite_seed = [0xAA; 16];
    match within("forged join", Duration::from_secs(15), b.net.join(&forged.encode())).await {
        Err(NetError::RejectedByPeer(m)) => assert!(m.to_lowercase().contains("not valid"), "{m}"),
        other => panic!("expected rejection, got {other:?}"),
    }

    // Expired at the host (the event expires, the code claims it does not).
    let seed = [0x55; 16];
    let invite_pk = *tincan_core::invite::invite_secret(&seed, &server).public().as_bytes();
    a.net.publish(server, Body::InviteCreate { invite_pk, expires_ms: now_ms() + 300, max_uses: 0 }).await.unwrap();
    let mut late = good.clone();
    late.invite_seed = seed;
    late.expires_ms = u64::MAX;
    tokio::time::sleep(Duration::from_millis(600)).await;
    match within("late join", Duration::from_secs(15), b.net.join(&late.encode())).await {
        Err(NetError::Rejected(m)) => assert!(m.contains("expired"), "{m}"),
        other => panic!("expected expiry at host, got {other:?}"),
    }
    assert!(b.net.conv_state(&server).is_none(), "nothing stored after rejections");

    // A JoinRequest whose invite signature was tampered with.
    let hlc = tincan_core::hlc::Hlc::new().now();
    let req = build::join_request(&b.identity, &good, &a.net.me(), "bob", 1, hlc, now_ms()).unwrap();
    let Ctl::JoinRequest { server: s_id, invite_pk, profile, ts_ms, mut invite_sig } = req else { panic!("join request") };
    invite_sig.0[0] ^= 1;
    let tampered = Ctl::JoinRequest { server: s_id, invite_pk, profile, ts_ms, invite_sig };
    let conn = crate::link::connect(b.net.endpoint(), &a.net.me(), Some(&hint(&a)), tincan_core::ALPN_SERVER, Duration::from_secs(3))
        .await
        .unwrap();
    let d = crate::link::exchange(conn, &tampered, Duration::from_secs(3)).await.unwrap();
    match &d.reply {
        Ctl::Reject { code: RejectCode::BadInvite, reason } => assert!(reason.contains("signature"), "{reason}"),
        other => panic!("expected BadInvite, got {other:?}"),
    }

    // Non-member Hello is rejected.
    let keys = tincan_core::keys::ServerKeys::derive(&[1; 32], &server);
    let hello = build::hello(&b.net.me(), &server, &keys.hello_key, &a.net.me(), build::HelloInfo::default());
    let conn = crate::link::connect(b.net.endpoint(), &a.net.me(), Some(&hint(&a)), tincan_core::ALPN_SERVER, Duration::from_secs(3))
        .await
        .unwrap();
    let d = crate::link::exchange(conn, &hello, Duration::from_secs(3)).await.unwrap();
    assert!(matches!(d.reply, Ctl::Reject { code: RejectCode::NotMember, .. }), "{:?}", d.reply);

    // The real invite still works afterwards.
    assert_eq!(within("good join", Duration::from_secs(15), b.net.join(&code)).await.unwrap(), server);
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leave_removes_local_copy_and_membership() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let server = cluster(&a, &[&b]).await;
    let mut b_events = b.net.subscribe();
    b.net.leave(server).await.unwrap();
    assert!(b.net.conv_state(&server).is_none());
    assert!(b.storage.conv(&server).unwrap().is_none());
    let mut left = false;
    while let Ok(ev) = b_events.try_recv() {
        if matches!(ev, NetEvent::Left { server: s } if s == server) {
            left = true;
        }
    }
    assert!(left);
    until("alice sees bob leave", Duration::from_secs(5), || !a.net.conv_state(&server).unwrap().is_member(&b.net.me())).await;
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

/// Q1: a member gets `SyncComplete` once the host's history is in: after every event of it
/// was announced, once per connection, again after a reconnect; never as host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_complete_follows_the_hosts_history() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let mut a_ev = a.net.subscribe();
    let server = a.net.create_server("Test server").await.unwrap();
    let ch = text_channel(&a, &server);
    let mut history = Vec::new();
    for i in 0..30 {
        history.push(say(&a, server, ch, &format!("history {i}")).await);
    }
    let invite = a.net.create_invite(server).await.unwrap();
    let mut b_ev = b.net.subscribe();
    within("join", Duration::from_secs(20), b.net.join(&invite)).await.expect("join");
    let collect = |rx: &mut tokio::sync::broadcast::Receiver<NetEvent>| {
        let mut seen = std::collections::HashSet::new();
        let mut done = 0;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                NetEvent::EventAdded { conv, event, .. } if conv == server && done == 0 => {
                    seen.insert(event.id);
                }
                NetEvent::SyncComplete { conv } if conv == server => done += 1,
                _ => {}
            }
        }
        (seen, done)
    };
    let (mut seen, mut done) = (std::collections::HashSet::new(), 0);
    until("SyncComplete at the joiner", Duration::from_secs(10), || {
        let (s, d) = collect(&mut b_ev);
        if done == 0 {
            seen.extend(s);
        }
        done += d;
        done > 0
    })
    .await;
    assert!(history.iter().all(|id| seen.contains(id)), "the whole history came before SyncComplete");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(collect(&mut b_ev).1, 0, "once per connection");
    // A reconnect after a restart: one more, after the catch-up (the missed message is
    // stored when SyncComplete is seen). Then one with nothing to catch up: the host's empty
    // last page still ends the sync.
    let mut b = b;
    for missed in [Some("while bob was away"), None] {
        let (b_id, b_store) = (b.identity.clone(), b.storage.clone());
        b.net.shutdown().await;
        drop(b);
        let missed = match missed {
            Some(text) => Some(say(&a, server, ch, text).await),
            None => None,
        };
        b = start(b_id, b_store, "bob", fast()).await;
        let mut b_ev = b.net.subscribe();
        let mut stored_at_complete = false;
        until("SyncComplete after the reconnect", Duration::from_secs(15), || {
            let done = collect(&mut b_ev).1 > 0;
            if done {
                stored_at_complete = missed.is_none_or(|id| has_event(&b, &server, &id));
            }
            done
        })
        .await;
        assert!(stored_at_complete, "the missed message is in when SyncComplete arrives");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(collect(&mut b_ev).1, 0, "once per connection");
    }
    // The host never gets one.
    assert_eq!(collect(&mut a_ev).1, 0, "no SyncComplete while hosting");
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

/// Q3: `with_state` reads the live state without a copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_state_reads_the_live_state() {
    let a = node("alice", fast()).await;
    let server = a.net.create_server("Test server").await.unwrap();
    let ch = text_channel(&a, &server);
    let id = say(&a, server, ch, "hello").await;
    let snap = a.net.conv_state(&server).unwrap();
    let (name, channels, has) = a.net.with_state(&server, |s| (s.name.clone(), s.sorted_channels().len(), s.message_authors.contains_key(&id))).unwrap();
    assert_eq!((name, channels, has), (snap.name.clone(), snap.sorted_channels().len(), true));
    assert!(a.net.with_state(&ConvId([9; 32]), |_| ()).is_none());
    a.net.shutdown().await;
}

/// Q4: a member that takes over the host role lists only who is really there from the first
/// moment it says so: never the host that just left (the debounced broadcast comes later).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn new_host_roster_never_lists_the_departed_host() {
    // A slow roster broadcast makes the stale window wide.
    let t = Timings { roster_debounce: Duration::from_secs(2), ..fast() };
    let a = node("alice", t.clone()).await;
    let b = node("bob", t).await;
    wire_to(&a, &[&b]);
    let server = cluster(&a, &[&b]).await;
    wait_succession(&a, &server, 1).await;
    let mut b_ev = b.net.subscribe();
    let leaving = a.net.clone();
    let shutdown = tokio::spawn(async move { leaving.shutdown().await });
    let bid = b.net.me();
    within("bob hosts", Duration::from_secs(10), async {
        loop {
            if let Ok(NetEvent::ConnectionChanged { server: s, info }) = b_ev.recv().await
                && s == server
                && matches!(info.state, ConnState::Solo | ConnState::Hosting)
                && info.host == Some(bid)
            {
                return;
            }
        }
    })
    .await;
    let r = b.net.roster(&server).expect("roster");
    assert_eq!(r.host, bid, "{r:?}");
    assert!(r.online.iter().all(|e| e.user != a.net.me()), "the departed host is still listed: {r:?}");
    assert!(!b.net.peer_online(&a.net.me()));
    shutdown.await.expect("shutdown");
    b.net.shutdown().await;
}

#[test]
fn net_is_send_sync() {
    fn check<T: Send + Sync + Clone>() {}
    check::<Net>();
}

/// APP-CORE-03 (net part): with per-server announcements each server's host learns only
/// what was announced for that server (Hello and later deltas), and lists me as a holder
/// only there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn announcements_are_per_server() {
    use std::collections::HashMap;

    use crate::Announced;
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire_to(&a, &[&b]);
    let (x, y, z) = (BlobHash([1; 32]), BlobHash([2; 32]), BlobHash([3; 32]));
    // Bob announces before joining (it goes in the Hello) and the global list says all.
    b.net.set_have(vec![x, y, z]);
    b.net.set_announced(HashMap::new());
    let s1 = cluster(&a, &[&b]).await;
    let s2 = cluster(&a, &[&b]).await;
    let holds = |server: ServerId, hash: BlobHash| {
        let a = a.net.clone();
        let bob = b.net.me();
        async move { a.who_has(&server, hash).await.iter().any(|(u, _)| *u == bob) }
    };
    for (s, h) in [(s1, x), (s1, y), (s2, x), (s2, y), (s1, z)] {
        assert!(!holds(s, h).await, "nothing announced yet ({h:?})");
    }
    // Deltas per server.
    b.net.set_announced(HashMap::from([(s1, Announced { have: vec![x], clips: vec![] }), (s2, Announced { have: vec![y], clips: vec![] })]));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(holds(s1, x).await && holds(s2, y).await) {
        assert!(Instant::now() < deadline, "the per-server deltas did not arrive");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!holds(s1, y).await, "server 1's host learns nothing of server 2's blob");
    assert!(!holds(s2, x).await, "server 2's host learns nothing of server 1's blob");
    assert!(!holds(s1, z).await && !holds(s2, z).await, "the unscoped list is not announced");
    // Removing one from a server takes it out there only.
    b.net.set_announced(HashMap::from([(s2, Announced { have: vec![y], clips: vec![] })]));
    let deadline = Instant::now() + Duration::from_secs(5);
    while holds(s1, x).await {
        assert!(Instant::now() < deadline, "the removal did not arrive");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(holds(s2, y).await);
    for n in [a, b] {
        n.net.shutdown().await;
    }
}
