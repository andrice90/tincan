//! Hardening tests: a member (or a host) running a modified client. Each test drives one
//! misbehaviour through the real protocol and checks that honest nodes are not hurt.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tincan_core::election::{Attestation, SignedAttestation, Status};
use tincan_core::event::{Body, SignedEvent};
use tincan_core::keys::Identity;
use tincan_core::state::dm_channel;
use tincan_core::storage::Storage;
use tincan_core::sync::Heads;
use tincan_core::wire::{Ctl, Roster, RosterEntry};
use tincan_core::{ConvId, EventId, ServerId, UserId, build, now_ms};
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::tests::{Node, cluster, fast, hint, host_of, node, start, text_channel, until, wait_succession, wire, wire_to, within};
use crate::timing::Timings;
use crate::{ConnState, NetError, NetEvent};

fn drain_count(rx: &mut broadcast::Receiver<NetEvent>, mut f: impl FnMut(&NetEvent) -> bool) -> usize {
    let mut n = 0;
    loop {
        match rx.try_recv() {
            Ok(e) => {
                if f(&e) {
                    n += 1;
                }
            }
            Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => return n,
        }
    }
}

async fn say(n: &Node, server: ServerId, text: &str) -> crate::Result<tincan_core::event::StoredEvent> {
    let ch = text_channel(n, &server);
    within("publish", Duration::from_secs(20), n.net.publish(server, Body::Message { channel: ch, text: text.into(), attachments: vec![] })).await
}

fn stored_by(n: &Node, server: &ServerId, author: &UserId, seq: u64) -> Option<EventId> {
    n.storage.events_range(server, author, seq, seq).ok()?.first().map(|e| e.id)
}

fn att(id: &Identity, server: ServerId, host: UserId, epoch: u64, issued_ms: u64) -> SignedAttestation {
    SignedAttestation::sign(id, Attestation { server, host, epoch, member: id.user(), issued_ms }).unwrap()
}

/// S1: a member claims to host a bigger partition (inflated count, score and epoch, forged
/// attestations), then says the host moved. The live host with real members stays host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s1_member_cannot_take_over_a_live_host() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &c, &m]);
    let server = cluster(&a, &[&b, &c, &m]).await;
    let epoch = a.net.connection(&server).epoch;
    let (mid, bid) = (m.net.me(), b.net.me());
    let claim = build::host_claim(&m.identity, &server, epoch + 10, None, now_ms()).unwrap();
    // Attestations Mallory can make: fresh keys (not members), itself, a forged "bob",
    // stale ones and ones for another epoch.
    let fake = Identity::generate();
    let mut attests: Vec<SignedAttestation> = (0..20).map(|_| att(&Identity::generate(), server, mid, epoch + 10, now_ms())).collect();
    attests.push(att(&m.identity, server, mid, epoch + 10, now_ms()));
    let mut forged = att(&fake, server, mid, epoch + 10, now_ms());
    forged.att.member = bid;
    attests.push(forged);
    attests.push(att(&b.identity, server, mid, epoch + 10, now_ms() - 10 * 60 * 1000));
    attests.push(att(&b.identity, server, mid, epoch + 9, now_ms()));
    assert!(m.net.send_ctl_to_host(&server, Ctl::HostCompare2 { claim: claim.clone(), attests }).await);
    // The legacy compare with self-reported numbers.
    assert!(m.net.send_ctl_to_host(&server, Ctl::HostCompare { claim, connected: 1000, score: 9999 }).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let moved = build::host_claim(&m.identity, &server, epoch + 20, None, now_ms()).unwrap();
    assert!(m.net.send_ctl_to_host(&server, Ctl::HostMoved { claim: moved, addr: hint(&m) }).await);
    // Again after the per-peer cooldown.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let claim = build::host_claim(&m.identity, &server, epoch + 30, None, now_ms()).unwrap();
    assert!(m.net.send_ctl_to_host(&server, Ctl::HostCompare { claim, connected: 1000, score: 9999 }).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let moved = build::host_claim(&m.identity, &server, epoch + 40, None, now_ms()).unwrap();
    assert!(m.net.send_ctl_to_host(&server, Ctl::HostMoved { claim: moved, addr: hint(&m) }).await);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(a.net.connection(&server).state, ConnState::Hosting, "the live host kept hosting");
    for n in [&b, &c] {
        assert_eq!(host_of(n, &server), Some(a.net.me()), "members stayed with the live host");
    }
    assert!(a.net.roster(&server).is_some_and(|r| r.online.len() == 4));
    // Honest members still work through it.
    let after = say(&b, server, "still here").await.expect("publish after the attack");
    until("fan-out after the attack", Duration::from_secs(5), || stored_by(&c, &server, &bid, after.event.seq).is_some()).await;
    for n in [a, b, c, m] {
        n.net.shutdown().await;
    }
}

/// S1 (honest path): a host whose member attested beats a solo host, so two honest
/// partitions still merge to one winner (the populated one).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s1_populated_partition_wins_a_merge_with_a_solo_host() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire(&[&a, &b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let ids: Vec<_> = [&a, &b, &c].iter().map(|n| (n.identity.clone(), n.storage.clone())).collect();
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
    let a = start(ids[0].0.clone(), ids[0].1.clone(), "alice", fast()).await;
    let b = start(ids[1].0.clone(), ids[1].1.clone(), "bob", fast()).await;
    let c = start(ids[2].0.clone(), ids[2].1.clone(), "carol", fast()).await;
    // a and c find each other; b is alone.
    wire(&[&a, &c]);
    until("a and c together, b solo", Duration::from_secs(15), || {
        let h = host_of(&a, &server);
        h.is_some() && h == host_of(&c, &server) && b.net.connection(&server).state == ConnState::Solo
    })
    .await;
    let pair_host = host_of(&a, &server).unwrap();
    // Let the member's attestation reach its host, then heal the partition.
    tokio::time::sleep(Duration::from_millis(300)).await;
    wire(&[&a, &b, &c]);
    until("one host again", Duration::from_secs(20), || [&a, &b, &c].iter().all(|n| host_of(n, &server) == Some(pair_host))).await;
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

/// E1: an unbounded self-signed epoch from a member is ignored (no overflow, no poisoning).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e1_huge_epochs_from_a_member_are_ignored() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &m]);
    let server = cluster(&a, &[&b, &m]).await;
    let before = a.net.connection(&server).epoch;
    let huge = build::host_claim(&m.identity, &server, u64::MAX, None, now_ms()).unwrap();
    assert!(m.net.send_ctl_to_host(&server, Ctl::HostCompare2 { claim: huge.clone(), attests: vec![] }).await);
    assert!(m.net.send_ctl_to_host(&server, Ctl::HostMoved { claim: huge, addr: hint(&m) }).await);
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The host's session is alive (it publishes) and its epoch did not jump.
    say(&a, server, "alive").await.expect("host session alive");
    let after = a.net.connection(&server).epoch;
    assert!(after <= before + 1, "epoch {before} -> {after}");
    assert_eq!(a.net.connection(&server).state, ConnState::Hosting);
    assert!(a.storage.conv(&server).unwrap().unwrap().last_epoch < 1000, "nothing poisoned on disk");
    for n in [a, b, m] {
        n.net.shutdown().await;
    }
}

/// E1 + DOS-04: a member's bare compare (no attested members) at a higher epoch never moves
/// the live host's epoch: the host wins and keeps its claim, its members keep theirs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e1_bare_compare_never_moves_the_host_epoch() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &m]);
    let server = cluster(&a, &[&b, &m]).await;
    // Let the members' attestations reach the host.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let epoch = a.net.connection(&server).epoch;
    for (i, lead) in [1u64, 500, 100_000].into_iter().enumerate() {
        if i > 0 {
            // Past the per-peer compare cooldown.
            tokio::time::sleep(Duration::from_millis(1200)).await;
        }
        let claim = build::host_claim(&m.identity, &server, epoch + lead, None, now_ms()).unwrap();
        assert!(m.net.send_ctl_to_host(&server, Ctl::HostCompare2 { claim, attests: vec![] }).await);
    }
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(a.net.connection(&server).epoch, epoch, "a bare compare bumped the host");
    assert_eq!(b.net.connection(&server).epoch, epoch);
    assert_eq!(a.net.connection(&server).state, ConnState::Hosting);
    assert_eq!(host_of(&b, &server), Some(a.net.me()));
    assert!(a.storage.conv(&server).unwrap().unwrap().last_epoch <= epoch, "nothing persisted");
    for n in [a, b, m] {
        n.net.shutdown().await;
    }
}

/// E1: an epoch pushed up to the ceiling (as a malicious host could) locks nobody out. The
/// ceiling needs no history, so a member that was offline meanwhile and a brand-new joiner
/// (who saw epoch 0) both accept the live host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e1_pushed_epoch_locks_nobody_out() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let server = cluster(&a, &[&b]).await;
    let old = b.net.connection(&server).epoch;
    let ids: Vec<_> = [&a, &b].iter().map(|n| (n.identity.clone(), n.storage.clone())).collect();
    for n in [a, b] {
        n.net.shutdown().await;
    }
    // Alice followed a host that pushed the epoch to the ceiling; Bob was offline then.
    let pushed = tincan_core::election::epoch_ceiling(now_ms()) - 10;
    let mut rec = ids[0].1.conv(&server).unwrap().unwrap();
    rec.last_epoch = pushed;
    ids[0].1.upsert_conv(&rec).unwrap();
    let a = start(ids[0].0.clone(), ids[0].1.clone(), "alice", fast()).await;
    until("alice hosts alone", Duration::from_secs(10), || a.net.connection(&server).state == ConnState::Solo).await;
    assert!(a.net.connection(&server).epoch > pushed);
    let b = start(ids[1].0.clone(), ids[1].1.clone(), "bob", fast()).await;
    assert!(b.net.connection(&server).epoch <= old + 1);
    wire(&[&a, &b]);
    until("the offline member rejoins", Duration::from_secs(15), || host_of(&b, &server) == Some(a.net.me())).await;
    // A new member who saw no epoch at all joins too.
    let invite = within("invite", Duration::from_secs(10), a.net.create_invite(server)).await.expect("invite");
    let j = node("joy", fast()).await;
    wire(&[&a, &b, &j]);
    within("join", Duration::from_secs(20), j.net.join(&invite)).await.expect("join");
    until("joiner connected", Duration::from_secs(10), || host_of(&j, &server) == Some(a.net.me())).await;
    until("joiner synced", Duration::from_secs(10), || {
        j.net.conv_state(&server).is_some_and(|s| s.sorted_channels().iter().any(|c| c.kind == tincan_core::event::ChannelKind::Text))
    })
    .await;
    say(&j, server, "hello").await.expect("publish after joining");
    for n in [a, b, j] {
        n.net.shutdown().await;
    }
}

/// N1: a host's bogus `your_next_seq` cannot push my seqs out of reach.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n1_host_cannot_raise_my_seq() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire(&[&a, &b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    a.net.inner().test.welcome_next_seq.store(1 << 60, Ordering::Relaxed);
    // Bob reconnects and gets the poisoned Welcome.
    let (bid, bstore) = (b.identity.clone(), b.storage.clone());
    b.net.shutdown().await;
    let b = start(bid, bstore, "bob", fast()).await;
    wire(&[&a, &b, &c]);
    until("bob back", Duration::from_secs(10), || host_of(&b, &server) == Some(a.net.me())).await;
    let ev = say(&b, server, "after a poisoned welcome").await.expect("publish works");
    assert!(ev.event.seq < 100, "seq {}", ev.event.seq);
    until("carol has it", Duration::from_secs(5), || stored_by(&c, &server, &b.net.me(), ev.event.seq) == Some(ev.id)).await;
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

/// N9: a host that stores my event but answers "refused" cannot make me sign another event
/// with the same seq; its reason text is cleaned before I see it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n9_refused_publish_never_reuses_its_seq() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire(&[&a, &b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let bid = b.net.me();
    *a.net.inner().test.refuse_publish.lock() = Some(format!("\u{1b}[31mACCOUNT LOCKED\u{7}{}", "!".repeat(5000)));
    let first = say(&b, server, "first").await;
    let text = match first {
        Err(NetError::RejectedByPeer(t)) => t,
        other => panic!("expected a refusal, got {other:?}"),
    };
    assert!(text.chars().count() <= 200 && !text.chars().any(char::is_control), "{text:?}");
    // The malicious host stored and fanned it out anyway.
    let seq = b.net.conv_state(&server).unwrap().next_seq(&bid);
    until("carol got the refused event", Duration::from_secs(5), || stored_by(&c, &server, &bid, seq).is_some()).await;
    let refused_id = stored_by(&c, &server, &bid, seq).unwrap();
    *a.net.inner().test.refuse_publish.lock() = None;
    let second = say(&b, server, "second").await.expect("publish after the refusal");
    assert_eq!(second.event.seq, seq + 1, "the refused event kept its seq");
    until("everyone has both", Duration::from_secs(10), || {
        [&a, &b, &c].iter().all(|n| stored_by(n, &server, &bid, seq) == Some(refused_id) && stored_by(n, &server, &bid, seq + 1) == Some(second.id))
    })
    .await;
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

/// N4: a first successor that answers "alive, not hosting" forever cannot keep the server
/// hostless: the next successor takes over within a bounded time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n4_successor_that_never_promotes_cannot_block_failover() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire_to(&a, &[&b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let succ = wait_succession(&a, &server, 2).await;
    until("members know the succession", Duration::from_secs(5), || {
        [&b, &c].iter().all(|n| n.net.roster(&server).is_some_and(|r| r.succession.len() >= 2))
    })
    .await;
    let (bad, good) = if succ[0] == b.net.me() { (&b, &c) } else { (&c, &b) };
    bad.net.inner().test.never_promote.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    a.net.crash();
    until("the next successor hosts", Duration::from_secs(25), || host_of(good, &server) == Some(good.net.me())).await;
    let took = t0.elapsed();
    eprintln!("failover past a successor that never promotes took {took:?}");
    assert!(took < Duration::from_secs(15), "took {took:?}");
    for n in [b, c] {
        n.net.shutdown().await;
    }
}

/// N2: a Typing flood is cut per member and channel, and nobody gets dropped for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n2_typing_flood_is_rate_limited_and_drops_nobody() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire(&[&a, &b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let ch = text_channel(&a, &server);
    let mut c_ev = c.net.subscribe();
    let mut a_ev = a.net.subscribe();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(1500) {
        for _ in 0..50 {
            let _ = b.net.send_ctl_to_host(&server, Ctl::Typing { channel: ch, by: b.net.me() }).await;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let at_c = drain_count(&mut c_ev, |e| matches!(e, NetEvent::Typing { .. }));
    let at_a = drain_count(&mut a_ev, |e| matches!(e, NetEvent::Typing { .. }));
    assert!(at_c <= 2 && at_a <= 2, "typing relayed {at_c} times to carol, {at_a} to the host");
    assert!(at_c >= 1, "a real typing notice still goes through");
    assert_eq!(host_of(&c, &server), Some(a.net.me()));
    assert!(a.net.roster(&server).is_some_and(|r| r.online.len() == 3));
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

/// N7: a SyncHave flood starts a bounded number of history syncs at the host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n7_synchave_flood_starts_few_syncs() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let server = cluster(&a, &[&b]).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = a.net.inner().test.syncs_started.load(Ordering::Relaxed);
    for _ in 0..200 {
        let _ = b.net.send_ctl_to_host(&server, Ctl::SyncHave { heads: Heads::default() }).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    let started = a.net.inner().test.syncs_started.load(Ordering::Relaxed) - before;
    assert!(started <= 5, "{started} syncs for 200 SyncHave frames");
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

/// N14 + N18: a malicious host's roster full of strangers, and typing notices naming
/// people who are not online, do not reach the member's state, storage or app.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n14_n18_member_filters_what_a_host_relays() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire(&[&a, &b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let real = a.net.roster(&server).unwrap();
    let fakes: Vec<UserId> = (0..40).map(|_| Identity::generate().user()).collect();
    let entry = |u: UserId, i: u16| RosterEntry {
        user: u,
        short_id: 100 + i,
        addr: hint(&a),
        since_ms: 1,
        voice: None,
        muted: false,
        deafened: false,
        sharing: None,
        direct: true,
        rtt_ms: 1,
    };
    let mut online = real.online.clone();
    online.extend(fakes.iter().enumerate().map(|(i, u)| entry(*u, i as u16)));
    let poisoned = Roster { epoch: real.epoch, rev: real.rev + 100, host: a.net.me(), online, succession: fakes[..4].to_vec() };
    assert!(a.net.broadcast_ctl(&server, Ctl::Roster(poisoned), None).await);
    // A roster too large to be honest is refused as a whole.
    let huge: Vec<RosterEntry> = (0..300).map(|i| entry(Identity::generate().user(), i)).collect();
    let flood = Roster { epoch: real.epoch, rev: real.rev + 200, host: a.net.me(), online: huge, succession: vec![] };
    let _ = a.net.broadcast_ctl(&server, Ctl::Roster(flood), None).await;
    let mut b_ev = b.net.subscribe();
    // Typing that names a stranger, and one that names an online member.
    let ch = text_channel(&a, &server);
    assert!(a.net.broadcast_ctl(&server, Ctl::Typing { channel: ch, by: fakes[0] }, None).await);
    tokio::time::sleep(Duration::from_millis(800)).await;
    let r = b.net.roster(&server).unwrap();
    assert!(r.online.iter().all(|e| !fakes.contains(&e.user)), "strangers in the member's roster");
    assert!(r.succession.iter().all(|u| !fakes.contains(u)));
    let peers = b.storage.peers(&server, 1000).unwrap();
    assert!(peers.iter().all(|p| !fakes.contains(&p.user)), "strangers persisted as peers");
    let typed = drain_count(&mut b_ev, |e| matches!(e, NetEvent::Typing { user, .. } if fakes.contains(user)));
    assert_eq!(typed, 0, "typing by a stranger was shown");
    assert!(a.net.broadcast_ctl(&server, Ctl::Typing { channel: ch, by: c.net.me() }, Some(c.net.me())).await);
    let cid = c.net.me();
    within("typing by an online member", Duration::from_secs(5), async {
        loop {
            if let Ok(NetEvent::Typing { user, .. }) = b_ev.recv().await
                && user == cid
            {
                return;
            }
        }
    })
    .await;
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

/// DOS-13: a member flapping its status gets the host to broadcast at most about one roster
/// per `roster_min_interval`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dos13_status_flapping_is_coalesced() {
    let t = Timings { roster_min_interval: Duration::from_secs(1), ..fast() };
    let a = node("alice", t.clone()).await;
    let b = node("bob", t.clone()).await;
    let c = node("carol", t).await;
    wire(&[&a, &b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    // Counted at the host: one RosterUpdated per roster it broadcasts.
    let mut a_ev = a.net.subscribe();
    let t0 = Instant::now();
    let mut i = 0u32;
    while t0.elapsed() < Duration::from_secs(2) {
        let st = Status { app_version: "x".into(), muted: i.is_multiple_of(2), deafened: i.is_multiple_of(3), hosting_ok: true, synced: true, ..Status::default() };
        let _ = b.net.send_ctl_to_host(&server, Ctl::Status(st)).await;
        i += 1;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let rosters = drain_count(&mut a_ev, |e| matches!(e, NetEvent::RosterUpdated { .. }));
    eprintln!("{rosters} roster broadcasts for {i} status flips");
    assert!(rosters <= 5, "{rosters} rosters for {i} status flips in 2 s");
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

/// N10 + DOS-12: an author's second event for a seq is detected and reported (with the
/// proof kept); a member forwarding a badly signed event is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n10_equivocation_is_detected_and_bad_signatures_drop_the_sender() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let server = cluster(&a, &[&b]).await;
    let first = say(&b, server, "the real message").await.expect("publish");
    let mut a_ev = a.net.subscribe();
    let ch = text_channel(&a, &server);
    let twin = build::event(&b.identity, server, first.event.seq, first.event.hlc + 1, Body::Message { channel: ch, text: "another version".into(), attachments: vec![] }).unwrap();
    assert!(b.net.send_ctl_to_host(&server, Ctl::Events { events: vec![twin], more: false }).await);
    let bid = b.net.me();
    let seq = within("equivocation reported", Duration::from_secs(5), async {
        loop {
            if let Ok(NetEvent::Equivocation { author, seq, conv }) = a_ev.recv().await
                && author == bid
                && conv == server
            {
                return seq;
            }
        }
    })
    .await;
    assert_eq!(seq, first.event.seq);
    let key = format!("net/equiv/{}/{}", server.to_hex(), bid.to_hex());
    assert!(a.storage.get_kv(&key).unwrap().is_some(), "proof kept");
    assert_eq!(stored_by(&a, &server, &bid, seq), Some(first.id), "the first version stays");
    // A badly signed event: the host drops the member that sent it.
    let next = build::event(&b.identity, server, first.event.seq + 1, first.event.hlc + 2, Body::Profile { name: "x".into() }).unwrap();
    let mut bad = SignedEvent { bytes: next.bytes.clone(), sig: next.sig };
    bad.sig.0[3] ^= 1;
    assert!(b.net.send_ctl_to_host(&server, Ctl::Events { events: vec![bad], more: false }).await);
    within("member dropped", Duration::from_secs(5), async {
        loop {
            if let Ok(NetEvent::MemberLinkDown { user, .. }) = a_ev.recv().await
                && user == bid
            {
                return;
            }
        }
    })
    .await;
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

/// N3: a stranger's DM connection stores nothing until it delivers a valid first message;
/// early (gap) events are not buffered; the number of stranger connections is capped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n3_strangers_store_nothing_until_a_valid_first_message() {
    let a = node("alice", fast()).await;
    let x = Identity::generate();
    let ep = crate::build_endpoint(&x, &crate::tests::cfg("x")).await.unwrap();
    let conv = ConvId::dm(&a.net.me(), &x.user());
    let mut a_ev = a.net.subscribe();
    let conn = crate::link::connect(&ep, &a.net.me(), Some(&hint(&a)), tincan_core::ALPN_DM, Duration::from_secs(3)).await.unwrap();
    let mut d = crate::link::exchange(conn, &Ctl::DmHello { conv, heads: Heads::default(), have: vec![] }, Duration::from_secs(3)).await.unwrap();
    assert!(matches!(d.reply, Ctl::DmHello { .. }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(a.net.conv_state(&conv).is_none(), "a DmHello alone created a conversation");
    assert!(a.storage.conv(&conv).unwrap().is_none());
    let msg = |seq: u64, text: &str| {
        build::event(&x, conv, seq, tincan_core::hlc::Hlc::new().now(), Body::Message { channel: dm_channel(&conv), text: text.into(), attachments: vec![] }).unwrap()
    };
    // Early events (seq 2..) are not buffered and open nothing.
    let early: Vec<SignedEvent> = (2..50).map(|s| msg(s, "early")).collect();
    crate::link::write_ctl(&mut d.send, &Ctl::Events { events: early, more: false }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(a.net.conv_state(&conv).is_none(), "gap events from a stranger opened a conversation");
    assert_eq!(drain_count(&mut a_ev, |e| matches!(e, NetEvent::DmOpened { .. })), 0);
    // A valid first message opens it.
    crate::link::write_ctl(&mut d.send, &Ctl::Events { events: vec![msg(1, "hello")], more: false }).await.unwrap();
    until("conversation opened", Duration::from_secs(5), || a.net.conv_state(&conv).is_some_and(|s| s.head(&x.user()) == 1)).await;
    // At most MAX_STRANGER_LINKS stranger connections at once: past that, the oldest is
    // closed to make room (it used to refuse newcomers, which let idle holders lock first
    // contact out; NET-SECURITY-6).
    let mut held = Vec::new();
    for i in 0..(crate::dm::MAX_STRANGER_LINKS + 1) {
        let id = Identity::generate();
        let ep = crate::build_endpoint(&id, &crate::tests::cfg(&format!("s{i}"))).await.unwrap();
        let conv = ConvId::dm(&a.net.me(), &id.user());
        let conn = crate::link::connect(&ep, &a.net.me(), Some(&hint(&a)), tincan_core::ALPN_DM, Duration::from_secs(3)).await.expect("connect");
        let d = crate::link::exchange(conn, &Ctl::DmHello { conv, heads: Heads::default(), have: vec![] }, Duration::from_secs(3)).await.expect("hello");
        held.push((ep, d));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let open = held.iter().filter(|(_, d)| d.conn.close_reason().is_none()).count();
    assert_eq!(open, crate::dm::MAX_STRANGER_LINKS, "the oldest stranger connection made room");
    assert!(held[0].1.conn.close_reason().is_some(), "the oldest went first");
    assert!(a.net.inner().dm.stranger_slots() <= crate::dm::MAX_STRANGER_LINKS);
    drop(held);
    a.net.shutdown().await;
}

/// Wait until every node in `nodes` follows the same host; true if that happened in time.
async fn one_host(server: &ServerId, nodes: &[&Node], timeout: Duration) -> bool {
    let t0 = Instant::now();
    loop {
        let h = host_of(nodes[0], server);
        if h.is_some() && nodes.iter().all(|n| host_of(n, server) == h) {
            return true;
        }
        if t0.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// S1 (honest path): two populated partitions of the same size (a host and one member each)
/// still merge into one when the network heals.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s1_equal_populated_partitions_merge() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    let d = node("dave", fast()).await;
    wire(&[&a, &b, &c, &d]);
    let server = cluster(&a, &[&b, &c, &d]).await;
    let ids: Vec<_> = [&a, &b, &c, &d].iter().map(|n| (n.identity.clone(), n.storage.clone())).collect();
    for n in [a, b, c, d] {
        n.net.shutdown().await;
    }
    let a = start(ids[0].0.clone(), ids[0].1.clone(), "alice", fast()).await;
    let b = start(ids[1].0.clone(), ids[1].1.clone(), "bob", fast()).await;
    let c = start(ids[2].0.clone(), ids[2].1.clone(), "carol", fast()).await;
    let d = start(ids[3].0.clone(), ids[3].1.clone(), "dave", fast()).await;
    // Two households: a+b and c+d find each other, not the other pair.
    wire(&[&a, &b]);
    wire(&[&c, &d]);
    until("two pairs", Duration::from_secs(20), || {
        let (h1, h2) = (host_of(&a, &server), host_of(&c, &server));
        h1.is_some()
            && h1 == host_of(&b, &server)
            && h2.is_some()
            && h2 == host_of(&d, &server)
            && h1 != h2
            && [&a, &b, &c, &d].iter().all(|n| matches!(n.net.connection(&server).state, ConnState::Member | ConnState::Hosting))
    })
    .await;
    // Let the members' attestations reach their hosts, then heal the network.
    tokio::time::sleep(Duration::from_millis(500)).await;
    wire(&[&a, &b, &c, &d]);
    assert!(one_host(&server, &[&a, &b, &c, &d], Duration::from_secs(30)).await, "equal-sized populated partitions never merged");
    for n in [a, b, c, d] {
        n.net.shutdown().await;
    }
}

/// E1 + S1: two solo hosts whose epochs are thousands apart (one was away while the server
/// went through many migrations) still merge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e1_solo_hosts_far_apart_in_epoch_merge() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let server = cluster(&a, &[&b]).await;
    let ids: Vec<_> = [&a, &b].iter().map(|n| (n.identity.clone(), n.storage.clone())).collect();
    for n in [a, b] {
        n.net.shutdown().await;
    }
    let mut rec = ids[1].1.conv(&server).unwrap().unwrap();
    rec.last_epoch += 5000;
    ids[1].1.upsert_conv(&rec).unwrap();
    let a = start(ids[0].0.clone(), ids[0].1.clone(), "alice", fast()).await;
    let b = start(ids[1].0.clone(), ids[1].1.clone(), "bob", fast()).await;
    until("both solo", Duration::from_secs(20), || [&a, &b].iter().all(|n| n.net.connection(&server).state == ConnState::Solo)).await;
    wire(&[&a, &b]);
    assert!(one_host(&server, &[&a, &b], Duration::from_secs(25)).await, "solo hosts far apart in epoch never merged");
    for n in [a, b] {
        n.net.shutdown().await;
    }
}

/// A host that comes back first and lacks recent admissions learns them from the members'
/// syncs; everyone ends up with one host. Several people join through one invite in a row
/// without being refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn returning_host_learns_recent_admissions() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    wire(&[&a, &b]);
    let server = cluster(&a, &[&b]).await;
    a.net.shutdown().await;
    until("bob hosts", Duration::from_secs(20), || matches!(b.net.connection(&server).state, ConnState::Solo | ConnState::Hosting)).await;
    let invite = within("invite", Duration::from_secs(10), b.net.create_invite(server)).await.expect("invite");
    let mut others = Vec::new();
    for name in ["c", "d", "e", "f", "g"] {
        let n = node(name, fast()).await;
        wire(&[&b, &n]);
        within("join", Duration::from_secs(20), n.net.join(&invite)).await.expect("several joins through one invite are not refused");
        others.push(n);
    }
    let bid = b.net.me();
    until("all with bob", Duration::from_secs(20), || others.iter().all(|n| host_of(n, &server) == Some(bid))).await;
    let mut ids = vec![(b.identity.clone(), b.storage.clone())];
    ids.extend(others.iter().map(|n| (n.identity.clone(), n.storage.clone())));
    b.net.shutdown().await;
    for n in others {
        n.net.shutdown().await;
    }
    let a = start(a.identity.clone(), a.storage.clone(), "alice", fast()).await;
    until("alice solo", Duration::from_secs(20), || a.net.connection(&server).state == ConnState::Solo).await;
    let mut back = Vec::new();
    for (i, (id, st)) in ids.into_iter().enumerate() {
        back.push(start(id, st, &format!("n{i}"), fast()).await);
    }
    let mut all: Vec<&Node> = vec![&a];
    all.extend(back.iter());
    wire(&all);
    assert!(one_host(&server, &all, Duration::from_secs(45)).await, "admitted members could not all reach one host");
    until("alice knows every admission", Duration::from_secs(10), || {
        a.net.conv_state(&server).is_some_and(|s| back.iter().all(|n| s.is_member(&n.net.me())))
    })
    .await;
    for n in back {
        n.net.shutdown().await;
    }
    a.net.shutdown().await;
}

/// S1 + TOCTOU: a host that lost a comparison while alone re-checks when the winner's
/// HostMoved arrives. Members that connected meanwhile are not handed to a peer that proved
/// none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s1_yield_is_rechecked_when_members_joined_meanwhile() {
    // Alice waits long enough for the winner's HostMoved that Bob surely reconnects first.
    let a = node("alice", Timings { merge_timeout: Duration::from_secs(15), ..fast() }).await;
    let b = node("bob", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &m]);
    let server = cluster(&a, &[&b, &m]).await;
    let (bid, bstore) = (b.identity.clone(), b.storage.clone());
    b.net.shutdown().await;
    until("alice hosts only mallory", Duration::from_secs(10), || a.net.roster(&server).is_some_and(|r| r.online.len() == 2)).await;
    let epoch = a.net.connection(&server).epoch;
    // Mallory "wins" against a host with no other member (0 against 0, higher epoch).
    let claim = build::host_claim(&m.identity, &server, epoch + 1, None, now_ms()).unwrap();
    assert!(m.net.send_ctl_to_host(&server, Ctl::HostCompare2 { claim, attests: vec![] }).await);
    // Bob comes back and connects to Alice before Mallory's HostMoved.
    let b = start(bid, bstore, "bob", fast()).await;
    wire(&[&a, &b, &m]);
    until("bob back with alice", Duration::from_secs(10), || host_of(&b, &server) == Some(a.net.me())).await;
    let moved = build::host_claim(&m.identity, &server, epoch + 2, None, now_ms()).unwrap();
    assert!(m.net.send_ctl_to_host(&server, Ctl::HostMoved { claim: moved, addr: hint(&m) }).await);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(a.net.connection(&server).state, ConnState::Hosting, "the host kept hosting");
    assert_eq!(host_of(&b, &server), Some(a.net.me()), "its member was not handed over");
    for n in [a, b, m] {
        n.net.shutdown().await;
    }
}

/// DOS (admissions): a member flooding admissions (gap seqs, and more than its budget) spends
/// only its own budget. Joins through another member's invite and another member's own
/// admissions still go through, and nobody is dropped for valid signatures.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admission_flood_spends_only_the_flooders_budget() {
    let a = node("alice", fast()).await;
    let x = node("xavier", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &x, &m]);
    let server = cluster(&a, &[&x, &m]).await;
    let invite = within("invite", Duration::from_secs(10), a.net.create_invite(server)).await.expect("invite");
    let invite_pk = *a.net.conv_state(&server).unwrap().invites.keys().next().unwrap();
    let mid = m.net.me();
    let head = m.net.conv_state(&server).unwrap().head(&mid);
    let admit = |seq: u64| {
        let body = Body::MemberAdmit { user: Identity::generate().user(), invite_pk, from_seq: 1 };
        build::event(&m.identity, server, seq, (now_ms() << 16) + seq, body).unwrap()
    };
    // Gap admissions (they would wait in the pending buffer).
    for i in 0..30 {
        assert!(m.net.send_ctl_to_host(&server, Ctl::Events { events: vec![admit(head + 1000 + i)], more: false }).await);
    }
    // And far more contiguous ones than the budget allows.
    let batch: Vec<SignedEvent> = (1..=60).map(|i| admit(head + i)).collect();
    assert!(m.net.send_ctl_to_host(&server, Ctl::Events { events: batch, more: false }).await);
    tokio::time::sleep(Duration::from_millis(800)).await;
    let state = a.net.conv_state(&server).unwrap();
    let admitted_by_m = state.head(&mid) - head;
    assert!(admitted_by_m <= 20, "{admitted_by_m} admissions from one member at once");
    assert!(admitted_by_m >= 1, "the budget admits some");
    assert_eq!(host_of(&m, &server), Some(a.net.me()), "valid signatures do not get anyone dropped");
    // Someone else's invite still admits, and another member's own admission still lands.
    let j = node("joy", fast()).await;
    wire(&[&a, &x, &m, &j]);
    within("join", Duration::from_secs(20), j.net.join(&invite)).await.expect("join through the owner's invite");
    let newcomer = Identity::generate().user();
    within("admission by xavier", Duration::from_secs(10), x.net.publish(server, Body::MemberAdmit { user: newcomer, invite_pk, from_seq: 1 }))
        .await
        .expect("xavier's own admission");
    until("xavier's admission at the host", Duration::from_secs(5), || a.net.conv_state(&server).is_some_and(|s| s.is_member(&newcomer))).await;
    for n in [a, x, m, j] {
        n.net.shutdown().await;
    }
}

/// Attest and Typing inputs: typing for a channel that does not exist relays nothing, and an
/// attestation with a bad signature drops the member (each one costs a verification).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typing_and_attest_inputs_are_checked() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &m]);
    let server = cluster(&a, &[&b, &m]).await;
    let mid = m.net.me();
    let mut b_ev = b.net.subscribe();
    for i in 0..40u8 {
        let _ = m.net.send_ctl_to_host(&server, Ctl::Typing { channel: EventId([i; 32]), by: mid }).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(drain_count(&mut b_ev, |e| matches!(e, NetEvent::Typing { .. })), 0, "typing for unknown channels was relayed");
    // Attestations are verified at most once per 5 s per member: wait out Mallory's real one.
    tokio::time::sleep(Duration::from_millis(5200)).await;
    let mut a_ev = a.net.subscribe();
    let epoch = a.net.connection(&server).epoch;
    let mut bad = att(&m.identity, server, a.net.me(), epoch, now_ms());
    bad.sig.0[5] ^= 1;
    assert!(m.net.send_ctl_to_host(&server, Ctl::Attest(bad)).await);
    within("member dropped", Duration::from_secs(5), async {
        loop {
            if let Ok(NetEvent::MemberLinkDown { user, .. }) = a_ev.recv().await
                && user == mid
            {
                return;
            }
        }
    })
    .await;
    for n in [a, b, m] {
        n.net.shutdown().await;
    }
}

/// NET-SECURITY-1: admissions signed by members admitted during this host role spend the
/// budget of whoever admitted them, on every path (relayed `Events` included). A member
/// relaying 200 admissions signed by a key it just admitted, and four more such keys
/// admitting 40 each, grow the server by at most one admission budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sybil_admissions_spend_their_admitters_budget() {
    // Everyone joins during Alice's host role, so for her nobody but herself is established.
    let a = node("alice", Timings { established_after: Duration::from_secs(3600), ..fast() }).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &m]);
    let server = cluster(&a, &[&m]).await;
    let invite_pk = *a.net.conv_state(&server).unwrap().invites.keys().next().unwrap();
    let before = a.net.conv_state(&server).unwrap().members.len();
    let sybils: Vec<Identity> = (0..5).map(|_| Identity::generate()).collect();
    for s in &sybils {
        let body = Body::MemberAdmit { user: s.user(), invite_pk, from_seq: 1 };
        within("mallory admits a key of hers", Duration::from_secs(10), m.net.publish(server, body)).await.expect("admitted");
    }
    let chain = |id: &Identity, n: u64| -> Vec<SignedEvent> {
        (1..=n)
            .map(|seq| {
                let body = Body::MemberAdmit { user: Identity::generate().user(), invite_pk, from_seq: 1 };
                build::event(id, server, seq, (now_ms() << 16) + seq, body).unwrap()
            })
            .collect()
    };
    let mut batch = chain(&sybils[0], 200);
    for s in &sybils[1..] {
        batch.extend(chain(s, 40));
    }
    assert!(m.net.send_ctl_to_host(&server, Ctl::Events { events: batch, more: false }).await);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let state = a.net.conv_state(&server).unwrap();
    let grown = state.members.len() - before;
    eprintln!("the server grew by {grown} members");
    assert!(grown <= 21, "{grown} new members from one member's budget");
    assert!(grown > sybils.len(), "within the budget some relayed admissions still apply");
    assert_eq!(host_of(&m, &server), Some(a.net.me()), "valid signatures get nobody dropped");
    for n in [a, m] {
        n.net.shutdown().await;
    }
}

/// NET-SECURITY-2: a member that admits a few keys of its own and has them attest for it
/// cannot take over a live host: only attesters the host stored long enough ago count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s1_sybil_attestations_do_not_move_a_live_host() {
    let a = node("alice", Timings { established_after: Duration::from_secs(2), ..fast() }).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &c, &m]);
    let server = cluster(&a, &[&b, &c, &m]).await;
    // Bob, Carol and Mallory become established for Alice, and their attestations arrive.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let epoch = a.net.connection(&server).epoch;
    let mid = m.net.me();
    let invite_pk = *a.net.conv_state(&server).unwrap().invites.keys().next().unwrap();
    // More fresh keys than Alice has other members.
    let sybils: Vec<Identity> = (0..4).map(|_| Identity::generate()).collect();
    for s in &sybils {
        let body = Body::MemberAdmit { user: s.user(), invite_pk, from_seq: 1 };
        within("mallory admits a key of hers", Duration::from_secs(10), m.net.publish(server, body)).await.expect("admitted");
    }
    until("alice knows them", Duration::from_secs(5), || a.net.conv_state(&server).is_some_and(|st| sybils.iter().all(|s| st.is_member(&s.user())))).await;
    let attests: Vec<SignedAttestation> = sybils.iter().map(|s| att(s, server, mid, epoch + 1, now_ms())).collect();
    let claim = build::host_claim(&m.identity, &server, epoch + 1, None, now_ms()).unwrap();
    assert!(m.net.send_ctl_to_host(&server, Ctl::HostCompare2 { claim, attests }).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let moved = build::host_claim(&m.identity, &server, epoch + 2, None, now_ms()).unwrap();
    assert!(m.net.send_ctl_to_host(&server, Ctl::HostMoved { claim: moved, addr: hint(&m) }).await);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(a.net.connection(&server).state, ConnState::Hosting, "the live host kept hosting");
    for n in [&b, &c] {
        assert_eq!(host_of(n, &server), Some(a.net.me()), "members stayed with the live host");
    }
    for n in [a, b, c, m] {
        n.net.shutdown().await;
    }
}

/// NET-SECURITY-3: a member pushing thousands of its own events in `Events` frames (outside
/// the publish budget) gets at most its own-events burst applied at once and more only as
/// the budget refills; honest members are not closed as too slow and still publish.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_frames_do_not_bypass_the_publish_budget() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &m]);
    let server = cluster(&a, &[&b, &m]).await;
    let (mid, bid) = (m.net.me(), b.net.me());
    let head = a.net.conv_state(&server).unwrap().head(&mid);
    let ch = text_channel(&a, &server);
    let flood: Vec<SignedEvent> = (head + 1..=head + 10_000)
        .map(|seq| {
            let body = Body::Message { channel: ch, text: format!("flood {seq}"), attachments: vec![] };
            build::event(&m.identity, server, seq, (now_ms() << 16) + seq, body).unwrap()
        })
        .collect();
    let mut a_ev = a.net.subscribe();
    for chunk in flood.chunks(2500) {
        assert!(m.net.send_ctl_to_host(&server, Ctl::Events { events: chunk.to_vec(), more: false }).await);
    }
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let applied = a.net.conv_state(&server).unwrap().head(&mid) - head;
    eprintln!("{applied} of 10000 applied at once");
    assert!((1000..=2020).contains(&applied), "{applied} own events applied at once");
    // Sent again later: a few more, at the publish rate.
    tokio::time::sleep(Duration::from_millis(2000)).await;
    for chunk in flood.chunks(2500) {
        assert!(m.net.send_ctl_to_host(&server, Ctl::Events { events: chunk.to_vec(), more: false }).await);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let later = a.net.conv_state(&server).unwrap().head(&mid) - head;
    assert!(later > applied && later <= applied + 40, "{later} after the refill");
    assert_eq!(drain_count(&mut a_ev, |e| matches!(e, NetEvent::MemberLinkDown { user, .. } if *user == bid)), 0, "bob was not dropped");
    assert_eq!(host_of(&b, &server), Some(a.net.me()));
    say(&b, server, "still here").await.expect("bob still publishes");
    for n in [a, b, m] {
        n.net.shutdown().await;
    }
}

/// NET-SECURITY-2 (honest path): with the attester age rule on and every member minutes old,
/// a solo host still rejoins the populated partition (it has nobody to hand over, so it
/// counts every valid attestation).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_solo_host_rejoins_a_young_server() {
    let t = || Timings { established_after: Duration::from_secs(3600), ..fast() };
    let a = node("alice", t()).await;
    let b = node("bob", t()).await;
    let c = node("carol", t()).await;
    wire(&[&a, &b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let ids: Vec<_> = [&a, &b, &c].iter().map(|n| (n.identity.clone(), n.storage.clone())).collect();
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
    let a = start(ids[0].0.clone(), ids[0].1.clone(), "alice", t()).await;
    let b = start(ids[1].0.clone(), ids[1].1.clone(), "bob", t()).await;
    let c = start(ids[2].0.clone(), ids[2].1.clone(), "carol", t()).await;
    wire(&[&a, &c]);
    until("a and c together, b solo", Duration::from_secs(15), || {
        let h = host_of(&a, &server);
        h.is_some() && h == host_of(&c, &server) && b.net.connection(&server).state == ConnState::Solo
    })
    .await;
    let pair_host = host_of(&a, &server).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    wire(&[&a, &b, &c]);
    until("one host again", Duration::from_secs(20), || [&a, &b, &c].iter().all(|n| host_of(n, &server) == Some(pair_host))).await;
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}
