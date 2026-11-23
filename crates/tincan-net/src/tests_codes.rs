//! Short codes end to end, in process: records go to a shared temporary folder
//! (`NetConfig::code_dir`), the way offline profiles on one machine share one.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tincan_core::code::{CodeKeys, CodeKind, ShortCode, code_invite, seal_invite};
use tincan_core::invite::Invite;
use tincan_core::keys::Identity;
use tincan_core::storage::MemStorage;
use tincan_core::{PROTOCOL_VERSION, ServerId};

use crate::codes::{CODES_OFF, CodeDir, NO_SUCH_FRIEND_CODE, NO_SUCH_INVITE_CODE, Written, code_packet, payload};
use crate::tests::{Node, cfg, fast, host_of, until, within};
use crate::{Net, NetError, build_endpoint};

async fn node_in(name: &str, dir: Option<&Path>) -> Node {
    let identity = Identity::generate();
    let storage = Arc::new(MemStorage::new());
    let mut c = cfg(name);
    c.code_dir = dir.map(Path::to_path_buf);
    let ep = build_endpoint(&identity, &c).await.expect("endpoint");
    let net = Net::start_with(ep, storage.clone(), identity.clone(), c, |b| b, fast()).await.expect("start");
    Node { net, storage, identity }
}

/// The invite form a user types: lowercase, no dash.
fn typed(code: ShortCode) -> String { code.compact().to_lowercase() }

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invite_code_join_and_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let a = node_in("alice", Some(dir.path())).await;
    let b = node_in("bob", Some(dir.path())).await;
    let c = node_in("carol", Some(dir.path())).await;
    let server = a.net.create_server("Codes").await.unwrap();
    assert_eq!(a.net.invite_code(server).await.unwrap(), None, "made on first use");
    let code = a.net.ensure_invite_code(server).await.unwrap().expect("owner makes a code");
    assert_eq!(a.net.ensure_invite_code(server).await.unwrap(), Some(code), "stable until rotated");

    // Bob knows nothing but the code: the record carries the host's address.
    let joined = within("join by code", Duration::from_secs(20), b.net.join(&typed(code))).await.unwrap();
    assert_eq!(joined, server);
    until("bob connected", Duration::from_secs(10), || host_of(&b, &server) == Some(a.net.me())).await;
    // Every member sees the same code; only the owner rotates.
    until("bob synced the code", Duration::from_secs(10), || b.net.conv_state(&server).is_some_and(|s| s.invites.len() >= 1)).await;
    assert_eq!(b.net.invite_code(server).await.unwrap(), Some(code));
    assert_eq!(b.net.ensure_invite_code(server).await.unwrap(), Some(code));
    assert!(matches!(b.net.rotate_invite_code(server).await, Err(NetError::Rejected(_))));

    let new = a.net.rotate_invite_code(server).await.unwrap();
    assert_ne!(new, code);
    assert_eq!(a.net.invite_code(server).await.unwrap(), Some(new));
    until("bob sees the new code", Duration::from_secs(10), || b.net.conv_state(&server).is_some_and(|s| s.invites.values().any(|i| i.revoked))).await;
    assert_eq!(b.net.invite_code(server).await.unwrap(), Some(new));

    // The old code admits nobody: its record is cleared, and the host refuses its key anyway.
    let old = within("join with the old code", Duration::from_secs(20), c.net.join(&code.to_string())).await;
    assert!(old.is_err(), "old code still worked");
    // Bob, a member, is unaffected; Carol gets in with the new code.
    assert!(b.net.conv_state(&server).is_some());
    assert_eq!(within("join with the new code", Duration::from_secs(20), c.net.join(&new.to_string())).await.unwrap(), server);

    // Hosts refuse the old invite key even when an old record still resolves.
    let keys = CodeKeys::derive(CodeKind::Invite, code);
    let inv = a.net.inner().replica(&server).and_then(|r| r.lock().keys.clone()).map(|k| Invite {
        v: PROTOCOL_VERSION,
        server,
        name_hint: "Codes".into(),
        invite_seed: keys.invite_seed,
        rdv_pk: *k.rdv_pk().as_bytes(),
        rdv_read_key: k.rdv_read_key,
        bootstrap: vec![(a.net.me(), crate::tests::hint(&a))],
        expires_ms: u64::MAX,
    });
    let d = node_in("dave", Some(dir.path())).await;
    let refused = within("join with the old key", Duration::from_secs(20), d.net.join_invite(inv.unwrap())).await;
    assert!(matches!(&refused, Err(NetError::RejectedByPeer(t)) if t.contains("revoked")), "{refused:?}");

    for n in [&a, &b, &c, &d] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn friend_code_resolves_and_rotates() {
    let dir = tempfile::tempdir().unwrap();
    let a = node_in("alice", Some(dir.path())).await;
    let b = node_in("bob", Some(dir.path())).await;
    let code = ShortCode::random();
    a.net.set_friend_code(Some(code));
    let mut found = None;
    for _ in 0..100 {
        if let Ok(u) = b.net.resolve_friend_code(code).await {
            found = Some(u);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(found, Some(a.net.me()));
    // Bob learned an address with it: a DM reaches Alice.
    assert!(b.net.inner().hint(&a.net.me()).is_some());

    let new = ShortCode::random();
    a.net.set_friend_code(Some(new));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let old = b.net.resolve_friend_code(code).await;
        let cur = b.net.resolve_friend_code(new).await;
        if matches!(&old, Err(NetError::Rejected(t)) if t == NO_SUCH_FRIEND_CODE) && cur.as_ref().ok() == Some(&a.net.me()) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "rotation never took effect: old {old:?}, new {cur:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    a.net.shutdown().await;
    b.net.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codes_off_without_a_folder_or_rendezvous() {
    let a = node_in("alice", None).await;
    let server = a.net.create_server("No codes").await.unwrap();
    assert!(matches!(a.net.ensure_invite_code(server).await, Err(NetError::Invalid(t)) if t == CODES_OFF));
    assert!(matches!(a.net.join("ABC-DEF").await, Err(NetError::Invalid(t)) if t == CODES_OFF));
    // The full invite still works as the fallback (decodes; nobody else to reach here).
    assert!(a.net.resolve_invite(&a.net.create_invite(server).await.unwrap()).await.is_ok());
    a.net.shutdown().await;
}

#[tokio::test]
async fn local_folder_write_rule_tombstone_and_collision() {
    let dir = tempfile::tempdir().unwrap();
    let store = CodeDir::Local(dir.path().to_path_buf());
    let keys = CodeKeys::derive(CodeKind::Invite, ShortCode::random());
    let base = |server: [u8; 32]| Invite {
        v: PROTOCOL_VERSION,
        server: ServerId::from_bytes(server),
        name_hint: "x".into(),
        invite_seed: [0; 16],
        rdv_pk: [1; 32],
        rdv_read_key: [2; 32],
        bootstrap: vec![],
        expires_ms: 0,
    };
    let mine = code_invite(&keys, &base([7; 32]));
    let theirs = code_invite(&keys, &base([8; 32]));
    let open = |sp: &iroh_dns::pkarr::SignedPacket| tincan_core::code::open_invite(&keys, &payload(sp)).ok();
    let readable = |sp: &iroh_dns::pkarr::SignedPacket| open(sp).is_some();
    let other_server = |sp: &iroh_dns::pkarr::SignedPacket| open(sp).is_some_and(|i| i.server != mine.server);
    let pk = keys.record_pk();

    // Someone else's fresh record under the same code is not overwritten.
    let sealed_theirs = seal_invite(&keys, &theirs).unwrap();
    assert_eq!(store.put(&keys.record_sk, &sealed_theirs, true, &readable, &|_| false).await, Written::Done(true, true));
    let sealed_mine = seal_invite(&keys, &mine).unwrap();
    assert_eq!(store.put(&keys.record_sk, &sealed_mine, true, &readable, &other_server).await, Written::Foreign);
    assert_eq!(store.newest(&pk, &readable).await.and_then(|sp| open(&sp)), Some(theirs.clone()));

    // A newer write wins; a tombstone hides every older record.
    let first = store.newest(&pk, &readable).await.unwrap().timestamp().as_micros();
    assert_eq!(store.put(&keys.record_sk, &sealed_mine, true, &readable, &|_| false).await, Written::Done(true, true));
    let second = store.newest(&pk, &readable).await.unwrap();
    assert!(second.timestamp().as_micros() > first);
    assert_eq!(open(&second), Some(mine));
    assert!(matches!(store.put(&keys.record_sk, &[], true, &|_| false, &|_| false).await, Written::Done(..)));
    let tomb = store.newest(&pk, &readable).await.unwrap();
    assert!(payload(&tomb).is_empty() && open(&tomb).is_none());

    // A packet built with an explicit seq carries it (the write rule's input).
    assert_eq!(code_packet(&keys.record_sk, &sealed_theirs, 42).unwrap().timestamp().as_micros(), 42);
    // Garbage in the folder is ignored, never trusted.
    std::fs::write(dir.path().join(format!("{}.pkarr", pk.to_z32())), b"garbage").unwrap();
    assert!(store.newest(&pk, &readable).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_codes_are_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let a = node_in("alice", Some(dir.path())).await;
    let code = ShortCode::random();
    assert!(matches!(a.net.join(&code.to_string()).await, Err(NetError::Rejected(t)) if t == NO_SUCH_INVITE_CODE));
    assert!(matches!(a.net.resolve_friend_code(code).await, Err(NetError::Rejected(t)) if t == NO_SUCH_FRIEND_CODE));
    // An invite code is not a friend code: separate KDF salts, separate records.
    let server = a.net.create_server("S").await.unwrap();
    let ic = a.net.ensure_invite_code(server).await.unwrap().unwrap();
    assert!(a.net.resolve_friend_code(ic).await.is_err());
    a.net.shutdown().await;
}
