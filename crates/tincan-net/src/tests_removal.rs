//! Removing a member (`Body::MemberRemove`): the honest path, then a removed member, a
//! forging member and a host that each misbehave. Same style as `tests_security`.

use std::time::Duration;

use tincan_core::event::{Body, SignedEvent, StoredEvent};
use tincan_core::storage::Storage;
use tincan_core::wire::Ctl;
use tincan_core::{EventId, ServerId, UserId, build, now_ms};
use tokio::sync::broadcast;

use crate::tests::{Node, cluster, fast, host_of, node, start, text_channel, until, wire, within};
use crate::{ConnState, NetError, NetEvent};

async fn say(n: &Node, server: ServerId, text: &str) -> crate::Result<StoredEvent> {
    let ch = text_channel(n, &server);
    within("publish", Duration::from_secs(20), n.net.publish(server, Body::Message { channel: ch, text: text.into(), attachments: vec![] })).await
}

fn stored(n: &Node, id: &EventId) -> bool { n.storage.event(id).ok().flatten().is_some() }

fn member(n: &Node, server: &ServerId, user: &UserId) -> bool { n.net.conv_state(server).is_some_and(|s| s.is_member(user)) }

fn valid(n: &Node, server: &ServerId, ev: &StoredEvent) -> bool {
    n.net.conv_state(server).is_some_and(|s| s.event_valid(&ev.event.author, ev.event.seq))
}

fn online(n: &Node, server: &ServerId, user: &UserId) -> bool { n.net.roster(server).is_some_and(|r| r.online.iter().any(|e| e.user == *user)) }

async fn removed_event(rx: &mut broadcast::Receiver<NetEvent>, server: ServerId) {
    within("a Removed event", Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(NetEvent::Removed { server: s }) if s == server => return,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("events closed: {e}"),
            }
        }
    })
    .await;
}

/// A message signed by `n` with its next seq, without going through any host.
fn sign_next(n: &Node, server: &ServerId, seq_ahead: u64, body: Body) -> SignedEvent {
    let st = n.net.conv_state(server).expect("state");
    let seq = st.next_seq(&n.net.me()) + seq_ahead;
    build::event(&n.identity, *server, seq, tincan_core::hlc::hlc_from_ms(now_ms()) + seq_ahead, body).expect("sign")
}

/// The owner (hosting) removes a member: everyone applies it, the host drops the member's
/// link and roster entry, the member learns it from the signed event and stops, its earlier
/// messages stay, and its publishes fail.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r1_owner_removes_a_member() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &m]);
    let server = cluster(&a, &[&b, &m]).await;
    let mid = m.net.me();
    let before = say(&m, server, "before").await.expect("publish");
    until("bob has it", Duration::from_secs(5), || stored(&b, &before.id)).await;
    let mut rx = m.net.subscribe();
    // Only the owner, never itself, never a non-member.
    assert!(matches!(b.net.remove_member(server, mid).await, Err(NetError::Invalid(_))));
    assert!(matches!(a.net.remove_member(server, a.net.me()).await, Err(NetError::Invalid(_))));
    let rm = within("remove", Duration::from_secs(10), a.net.remove_member(server, mid)).await.expect("remove");
    assert_eq!(rm.event.body, Body::MemberRemove { user: mid, last_seq: before.event.seq });
    removed_event(&mut rx, server).await;
    assert!(m.net.is_removed(&server));
    until("everyone applied it", Duration::from_secs(5), || !member(&a, &server, &mid) && !member(&b, &server, &mid)).await;
    until("the host dropped the link", Duration::from_secs(5), || !online(&a, &server, &mid)).await;
    until("bob's roster too", Duration::from_secs(5), || !online(&b, &server, &mid)).await;
    until("mallory's session stopped", Duration::from_secs(5), || m.net.connection(&server).state == ConnState::Offline).await;
    // Earlier messages stay valid everywhere, mallory's copy included.
    for n in [&a, &b, &m] {
        assert!(valid(n, &server, &before));
    }
    // Publishing fails at once.
    assert!(say(&m, server, "after").await.is_err());
    // Everyone else carries on.
    let ok = say(&b, server, "still fine").await.expect("bob publishes");
    until("alice has it", Duration::from_secs(5), || stored(&a, &ok.id)).await;
    // A restart does not reconnect it: the server stays readable, offline.
    let (mid_id, m_store) = (m.identity.clone(), m.storage.clone());
    m.net.shutdown().await;
    let m = start(mid_id, m_store, "mallory", fast()).await;
    assert!(m.net.is_removed(&server));
    assert!(m.net.conv_state(&server).is_some());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!online(&a, &server, &mid));
    assert_eq!(m.net.connection(&server).state, ConnState::Offline);
    for n in [a, b, m] {
        n.net.shutdown().await;
    }
}

/// A member forging a removal: refused by its own client, refused by the host when sent
/// raw, and rejected by every replica that receives it anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r2_non_owner_cannot_remove() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    wire(&[&a, &b, &c]);
    let server = cluster(&a, &[&b, &c]).await;
    let cid = c.net.me();
    assert!(matches!(b.net.publish(server, Body::MemberRemove { user: cid, last_seq: 0 }).await, Err(NetError::Invalid(_))));
    let forged = sign_next(&b, &server, 0, Body::MemberRemove { user: cid, last_seq: 0 });
    assert!(b.net.send_ctl_to_host(&server, Ctl::Publish { event: forged.clone() }).await);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!stored(&a, &forged.id()));
    // A host relaying it changes nothing either.
    assert!(a.net.broadcast_ctl(&server, Ctl::Events { events: vec![forged.clone()], more: false }, None).await);
    tokio::time::sleep(Duration::from_millis(500)).await;
    for n in [&a, &b, &c] {
        assert!(member(n, &server, &cid));
        assert!(!stored(n, &forged.id()));
    }
    assert_eq!(host_of(&c, &server), Some(a.net.me()), "carol stays connected");
    let ok = say(&c, server, "still here").await.expect("carol publishes");
    until("bob has it", Duration::from_secs(5), || stored(&b, &ok.id)).await;
    for n in [a, b, c] {
        n.net.shutdown().await;
    }
}

/// A host (here the owner's node, driven like a modified client) replays a removed member's
/// events: what the member signed after the removal, its old admission and a self-admission
/// at a void seq. Members store them for seq continuity, but they stay void: the member is
/// still removed and its later messages are hidden. The removed member keeps publishing on
/// its own: nobody stores anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r3_replayed_and_continued_events_stay_void() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &m]);
    let server = cluster(&a, &[&b, &m]).await;
    let mid = m.net.me();
    // Mallory signs ahead: a message and a re-admission of itself (its invite key unknown,
    // so with the owner's invite as seen in the log).
    let invite_pk = a.net.conv_state(&server).unwrap().invites.keys().next().copied().expect("an invite");
    let mut rx = m.net.subscribe();
    within("remove", Duration::from_secs(10), a.net.remove_member(server, mid)).await.expect("remove");
    removed_event(&mut rx, server).await;
    // Still publishing: the host is gone for it, so this cannot go anywhere.
    assert!(say(&m, server, "let me back in").await.is_err());
    let void_msg = sign_next(&m, &server, 0, Body::Message { channel: text_channel(&m, &server), text: "void".into(), attachments: vec![] });
    let self_admit = sign_next(&m, &server, 1, Body::MemberAdmit { user: mid, invite_pk, from_seq: 3 + m.net.conv_state(&server).unwrap().head(&mid) });
    // Mallory dials the host directly with a Publish: refused (it is not a member).
    assert!(!m.net.send_ctl_to_host(&server, Ctl::Publish { event: void_msg.clone() }).await);
    // The old admission, from alice's own storage.
    let old_admit = a
        .storage
        .state_events(&server)
        .unwrap()
        .into_iter()
        .find(|e| matches!(&e.event.body, Body::MemberAdmit { user, .. } if *user == mid))
        .expect("old admit")
        .signed;
    assert!(a.net.broadcast_ctl(&server, Ctl::Events { events: vec![old_admit, void_msg.clone(), self_admit.clone()], more: false }, None).await);
    until("bob stored the replay", Duration::from_secs(5), || stored(&b, &self_admit.id())).await;
    let st = b.net.conv_state(&server).unwrap();
    assert!(st.is_removed(&mid) && !st.is_member(&mid), "still removed");
    let void_ev = StoredEvent::from_signed(void_msg).unwrap();
    assert!(!valid(&b, &server, &void_ev), "the message is void");
    until("bob's roster drops it", Duration::from_secs(5), || !online(&b, &server, &mid)).await;
    for n in [a, b, m] {
        n.net.shutdown().await;
    }
}

/// A removed member comes back only with an invite that still works: once through the
/// original (unlimited) invite, then refused after the owner revoked it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r4_rejoin_needs_a_current_invite() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &m]);
    let server = a.net.create_server("Test server").await.expect("create");
    let invite = a.net.create_invite(server).await.expect("invite");
    for n in [&b, &m] {
        within("join", Duration::from_secs(20), n.net.join(&invite)).await.expect("join");
    }
    let mid = m.net.me();
    until("mallory connected", Duration::from_secs(10), || host_of(&m, &server) == Some(a.net.me())).await;
    until("history synced", Duration::from_secs(10), || m.net.conv_state(&server).is_some_and(|s| !s.sorted_channels().is_empty())).await;
    let before = say(&m, server, "before").await.expect("publish");
    let mut rx = m.net.subscribe();
    within("remove", Duration::from_secs(10), a.net.remove_member(server, mid)).await.expect("remove");
    removed_event(&mut rx, server).await;
    // Back through the same invite (the owner's: a removal cuts only the removed member's own).
    let back = within("rejoin", Duration::from_secs(20), m.net.join(&invite)).await.expect("rejoin");
    assert_eq!(back, server);
    assert!(!m.net.is_removed(&server));
    until("a member again", Duration::from_secs(10), || member(&a, &server, &mid) && member(&b, &server, &mid)).await;
    until("connected again", Duration::from_secs(10), || host_of(&m, &server) == Some(a.net.me())).await;
    until("history synced", Duration::from_secs(10), || m.net.conv_state(&server).is_some_and(|s| !s.sorted_channels().is_empty())).await;
    let again = say(&m, server, "back").await.expect("publish after rejoin");
    until("bob has it", Duration::from_secs(5), || stored(&b, &again.id)).await;
    assert!(valid(&b, &server, &again) && valid(&b, &server, &before));
    assert!(again.event.seq > before.event.seq, "seqs continue after the removal");
    // Removed again, and the owner rotates the invite: no way back in.
    let mut rx = m.net.subscribe();
    within("remove again", Duration::from_secs(10), a.net.remove_member(server, mid)).await.expect("remove");
    removed_event(&mut rx, server).await;
    let invite_pk = tincan_core::invite::Invite::decode(&invite).unwrap().invite_pk();
    a.net.publish(server, Body::InviteRevoke { invite_pk }).await.expect("revoke");
    let refused = within("rejoin refused", Duration::from_secs(30), m.net.join(&invite)).await;
    assert!(refused.is_err(), "a revoked invite does not let a removed member back");
    assert!(!member(&a, &server, &mid));
    for n in [a, b, m] {
        n.net.shutdown().await;
    }
}

/// A removed member cannot come back through an invite it made while still a member: the
/// host refuses it, and the admit would not count anyway. Another member's current invite
/// brings it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r7_own_invite_does_not_undo_a_removal() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &m]);
    let server = a.net.create_server("Test server").await.expect("create");
    let invite = a.net.create_invite(server).await.expect("invite");
    for n in [&b, &m] {
        within("join", Duration::from_secs(20), n.net.join(&invite)).await.expect("join");
    }
    let mid = m.net.me();
    until("mallory connected", Duration::from_secs(10), || host_of(&m, &server) == Some(a.net.me())).await;
    until("history synced", Duration::from_secs(10), || m.net.conv_state(&server).is_some_and(|s| !s.sorted_channels().is_empty())).await;
    let own = within("mallory's invite", Duration::from_secs(20), m.net.create_invite(server)).await.expect("invite");
    let own_pk = tincan_core::invite::Invite::decode(&own).unwrap().invite_pk();
    until("alice has it", Duration::from_secs(10), || a.net.conv_state(&server).is_some_and(|s| s.invite_status(&own_pk, now_ms()).is_ok())).await;
    let mut rx = m.net.subscribe();
    within("remove", Duration::from_secs(10), a.net.remove_member(server, mid)).await.expect("remove");
    removed_event(&mut rx, server).await;
    let refused = within("rejoin refused", Duration::from_secs(30), m.net.join(&own)).await;
    assert!(refused.is_err(), "a removed member came back through its own invite");
    assert!(m.net.is_removed(&server));
    assert!(!member(&a, &server, &mid) && !member(&b, &server, &mid));
    // Bob's current invite works.
    let bobs = within("bob's invite", Duration::from_secs(20), b.net.create_invite(server)).await.expect("invite");
    let back = within("rejoin", Duration::from_secs(30), m.net.join(&bobs)).await.expect("rejoin");
    assert_eq!(back, server);
    until("a member again", Duration::from_secs(10), || member(&a, &server, &mid) && member(&b, &server, &mid)).await;
    for n in [a, b, m] {
        n.net.shutdown().await;
    }
}

/// The removal is in the signed log, so a new host enforces it too. Mallory is offline when
/// removed; after the owner hands off, Mallory's reconnect is refused by the new host with the
/// owner-signed removal, which Mallory checks and applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r5_new_host_enforces_a_removal_made_while_offline() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &m]);
    let server = cluster(&a, &[&b, &m]).await;
    let mid = m.net.me();
    let (m_id, m_store) = (m.identity.clone(), m.storage.clone());
    m.net.shutdown().await;
    within("remove", Duration::from_secs(10), a.net.remove_member(server, mid)).await.expect("remove");
    until("bob applied it", Duration::from_secs(5), || !member(&b, &server, &mid)).await;
    // The owner goes away; bob takes over.
    a.net.shutdown().await;
    until("bob hosts", Duration::from_secs(10), || host_of(&b, &server) == Some(b.net.me())).await;
    let m = start(m_id, m_store, "mallory", fast()).await;
    m.net.add_addr_hint(b.net.me(), crate::tests::hint(&b));
    until("mallory learns it was removed", Duration::from_secs(15), || m.net.is_removed(&server)).await;
    assert!(!online(&b, &server, &mid));
    until("mallory stops", Duration::from_secs(5), || m.net.connection(&server).state == ConnState::Offline).await;
    for n in [b, m] {
        n.net.shutdown().await;
    }
}

/// The owner is a member, not the host: its removal goes through the host, which enforces
/// it. Then the owner removes the host itself, which hands the server over and stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r6_removal_through_a_host_and_of_the_host() {
    let a = node("alice", fast()).await;
    let b = node("bob", fast()).await;
    let c = node("carol", fast()).await;
    let m = node("mallory", fast()).await;
    wire(&[&a, &b, &c, &m]);
    let server = cluster(&a, &[&b, &c, &m]).await;
    // Alice restarts: someone else hosts, Alice comes back as a member.
    let (a_id, a_store) = (a.identity.clone(), a.storage.clone());
    a.net.shutdown().await;
    until("a new host", Duration::from_secs(10), || host_of(&b, &server).is_some_and(|h| h != a_id.user())).await;
    let a = start(a_id, a_store, "alice", fast()).await;
    wire(&[&a, &b, &c, &m]);
    let host = host_of(&b, &server).unwrap();
    until("alice is a member", Duration::from_secs(15), || host_of(&a, &server) == Some(host)).await;
    until("alice synced", Duration::from_secs(10), || a.net.conv_state(&server).is_some_and(|s| !s.sorted_channels().is_empty())).await;
    let mid = m.net.me();
    let host_node = [&b, &c, &m].into_iter().find(|n| n.net.me() == host).expect("host node");
    if host != mid {
        let mut rx = m.net.subscribe();
        within("remove", Duration::from_secs(10), a.net.remove_member(server, mid)).await.expect("remove through the host");
        removed_event(&mut rx, server).await;
        until("the host dropped mallory", Duration::from_secs(5), || !online(host_node, &server, &mid)).await;
    }
    // Now the host itself.
    let mut rx = host_node.net.subscribe();
    within("remove the host", Duration::from_secs(10), a.net.remove_member(server, host)).await.expect("remove the host");
    removed_event(&mut rx, server).await;
    until("someone else hosts", Duration::from_secs(15), || host_of(&a, &server).is_some_and(|h| h != host)).await;
    until("the old host is out", Duration::from_secs(10), || !online(&a, &server, &host)).await;
    let ok = say(&a, server, "after").await.expect("owner still publishes");
    assert!(valid(&a, &server, &ok));
    assert!(host_node.net.is_removed(&server));
    for n in [a, b, c, m] {
        n.net.shutdown().await;
    }
}
