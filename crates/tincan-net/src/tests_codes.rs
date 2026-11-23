//! Short codes end to end, in process: records go to a shared temporary folder
//! (`NetConfig::code_dir`), the way offline profiles on one machine share one.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use iroh_dns::pkarr::SignedPacket;
use tincan_core::code::{CodeKeys, CodeKind, FriendCode, FriendRecord, InviteRecord, ShortCode, invite_commit, verify_invite};
use tincan_core::invite::Invite;
use tincan_core::keys::{Identity, random_bytes};
use tincan_core::storage::MemStorage;
use tincan_core::{PROTOCOL_VERSION, ServerId, now_ms};

use crate::codes::{CODES_OFF, Check, CodeDir, NO_SUCH_FRIEND_CODE, NO_SUCH_INVITE_CODE, Written, code_packet, payload, publish_friend_record};
use crate::tests::{Node, cfg, fast, hint, host_of, until, within};
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

fn no_veto(_: &SignedPacket) -> Option<Check> { None }

/// Write `sealed` into `code`'s slot the way any holder of the code can.
async fn overwrite(dir: &Path, kind: CodeKind, code: ShortCode, sealed: &[u8]) {
    let keys = CodeKeys::derive(kind, code);
    let out = CodeDir::Local(dir.to_path_buf()).put(&keys.record_sk, sealed, true, &|_| false, &no_veto).await;
    assert_eq!(out, Written::Done(true, true));
}

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
    until("bob synced the code", Duration::from_secs(10), || b.net.conv_state(&server).is_some_and(|s| !s.invites.is_empty())).await;
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
    let k = a.net.inner().replica(&server).and_then(|r| r.lock().keys.clone()).unwrap();
    let st = a.net.conv_state(&server).unwrap();
    let old_inv = st.invites.values().find(|i| i.revoked).unwrap();
    let marker = tincan_core::code::open_marker(old_inv.max_uses).unwrap();
    let (seed, _) = tincan_core::code::invite_material(&k.code_key, marker);
    let inv = Invite {
        v: PROTOCOL_VERSION,
        server,
        name_hint: "Codes".into(),
        invite_seed: seed,
        rdv_pk: *k.rdv_pk().as_bytes(),
        rdv_read_key: k.rdv_read_key,
        bootstrap: vec![(a.net.me(), hint(&a))],
        expires_ms: u64::MAX,
    };
    assert_eq!(inv.invite_pk(), old_inv.invite_pk);
    let d = node_in("dave", Some(dir.path())).await;
    let refused = within("join with the old key", Duration::from_secs(20), d.net.join_invite(inv)).await;
    assert!(matches!(&refused, Err(NetError::RejectedByPeer(t)) if t.contains("revoked")), "{refused:?}");

    for n in [&a, &b, &c, &d] {
        n.net.shutdown().await;
    }
}

/// A member (who knows the code, so holds the slot key and the read key) writes a record
/// pointing the code at a lookalike server. Joiners never follow it; the host's next write
/// takes the slot back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forged_invite_record_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let a = node_in("alice", Some(dir.path())).await;
    let m = node_in("mallory", Some(dir.path())).await;
    let j = node_in("joiner", Some(dir.path())).await;
    let server = a.net.create_server("Real").await.unwrap();
    let code = a.net.ensure_invite_code(server).await.unwrap().unwrap();
    let lookalike = m.net.create_server("Real").await.unwrap();
    let mkeys = m.net.inner().replica(&lookalike).and_then(|r| r.lock().keys.clone()).unwrap();
    // Mallory's full invite to her lookalike, dressed as the code's record.
    let base = Invite {
        v: PROTOCOL_VERSION,
        server: lookalike,
        name_hint: "Real".into(),
        invite_seed: random_bytes(),
        rdv_pk: *mkeys.rdv_pk().as_bytes(),
        rdv_read_key: mkeys.rdv_read_key,
        bootstrap: vec![(m.net.me(), hint(&m))],
        expires_ms: u64::MAX,
    };
    let forged = InviteRecord::new(&base, random_bytes());
    assert!(!verify_invite(code, &forged));
    let keys = CodeKeys::derive(CodeKind::Invite, code);
    overwrite(dir.path(), CodeKind::Invite, code, &forged.seal(&keys).unwrap()).await;
    let r = within("resolve the forged slot", Duration::from_secs(20), j.net.resolve_invite(&code.to_string())).await;
    assert!(matches!(&r, Err(NetError::Rejected(t)) if t == NO_SUCH_INVITE_CODE), "followed a forged record: {r:?}");

    // The publisher overwrites a record that does not verify (no collision veto for junk).
    let shared = a.net.inner().sessions.lock().get(&server).map(|s| s.shared.clone());
    let active = crate::codes::active_invite_code(a.net.inner(), &server).await.unwrap();
    let out = crate::codes::publish_invite_record(a.net.inner(), &server, &active, shared.as_deref(), true).await;
    assert_eq!(out, Written::Done(true, true));
    assert_eq!(within("join after the rewrite", Duration::from_secs(20), j.net.join(&code.to_string())).await.unwrap(), server);
    for n in [&a, &m, &j] {
        n.net.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn friend_code_resolves_rotates_and_rejects_forgeries() {
    let dir = tempfile::tempdir().unwrap();
    let a = node_in("alice", Some(dir.path())).await;
    let b = node_in("bob", Some(dir.path())).await;
    let fc = FriendCode::generate(&a.net.me());
    a.net.set_friend_code(Some(fc));
    let resolve = |code: ShortCode| b.net.resolve_friend_code(code);
    let mut found = None;
    for _ in 0..100 {
        if let Ok(u) = resolve(fc.code).await {
            found = Some(u);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(found, Some(a.net.me()));
    // Bob learned an address with it: a DM reaches Alice.
    assert!(b.net.inner().hint(&a.net.me()).is_some());

    // Mallory got the code and writes her own validly signed record into the slot (with
    // Alice's nonce, or her own): it does not commit to the code, so nobody follows it.
    let mallory = Identity::generate();
    let keys = CodeKeys::derive(CodeKind::Friend, fc.code);
    for nonce in [fc.nonce, random_bytes()] {
        let rec = FriendRecord::new(&mallory, &FriendCode { code: fc.code, nonce }, &keys, &hint(&b), now_ms());
        overwrite(dir.path(), CodeKind::Friend, fc.code, &rec.seal(&keys).unwrap()).await;
        let r = resolve(fc.code).await;
        assert!(matches!(&r, Err(NetError::Rejected(t)) if t == NO_SUCH_FRIEND_CODE), "followed a forged record: {r:?}");
    }
    // Alice's next refresh takes the slot back (her publisher overwrites what does not verify).
    assert_eq!(publish_friend_record(a.net.inner(), fc, true).await, Written::Done(true, true));
    assert_eq!(resolve(fc.code).await.unwrap(), a.net.me());

    let new = FriendCode::generate(&a.net.me());
    a.net.set_friend_code(Some(new));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let old = resolve(fc.code).await;
        let cur = resolve(new.code).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_folder_write_rule_tombstone_and_collision() {
    let dir = tempfile::tempdir().unwrap();
    let store = CodeDir::Local(dir.path().to_path_buf());
    let base = |server: ServerId| Invite {
        v: PROTOCOL_VERSION,
        server,
        name_hint: "x".into(),
        invite_seed: random_bytes(),
        rdv_pk: [1; 32],
        rdv_read_key: [2; 32],
        bootstrap: vec![],
        expires_ms: 0,
    };
    // A genuine collision (two servers whose commitments give one code) cannot be searched
    // for here, so the veto is exercised with a check that says "verifies".
    let mine = InviteRecord::new(&base(ServerId::from_bytes([7; 32])), random_bytes());
    let code = invite_commit(&mine.invite.server, &mine.invite.invite_pk(), &mine.nonce);
    let keys = CodeKeys::derive(CodeKind::Invite, code);
    let theirs = InviteRecord::new(&base(ServerId::from_bytes([8; 32])), random_bytes());
    let open = |sp: &SignedPacket| InviteRecord::open(&keys, &payload(sp)).ok();
    let readable = |sp: &SignedPacket| open(sp).is_some();
    let mine_server = mine.invite.server;
    let ok_veto = |sp: &SignedPacket| -> Option<Check> { open(sp).filter(|r| r.invite.server != mine_server).map(|_| Box::new(|| true) as Check) };
    let pk = keys.record_pk();

    let sealed_theirs = theirs.seal(&keys).unwrap();
    assert_eq!(store.put(&keys.record_sk, &sealed_theirs, true, &readable, &no_veto).await, Written::Done(true, true));
    let sealed_mine = mine.seal(&keys).unwrap();
    assert_eq!(store.put(&keys.record_sk, &sealed_mine, true, &readable, &ok_veto).await, Written::Foreign);
    assert_eq!(store.candidates(&pk, &readable).await.first().and_then(open), Some(theirs.clone()));

    // A newer write wins; a tombstone replaces the record in the folder.
    let first = store.candidates(&pk, &readable).await[0].timestamp().as_micros();
    assert_eq!(store.put(&keys.record_sk, &sealed_mine, true, &readable, &no_veto).await, Written::Done(true, true));
    let second = store.candidates(&pk, &readable).await.remove(0);
    assert!(second.timestamp().as_micros() > first);
    assert_eq!(open(&second), Some(mine));
    assert!(matches!(store.put(&keys.record_sk, &[], true, &|_| false, &no_veto).await, Written::Done(..)));
    let tomb = store.candidates(&pk, &readable).await.remove(0);
    assert!(payload(&tomb).is_empty() && open(&tomb).is_none());

    // A packet built with an explicit seq carries it (the write rule's input).
    assert_eq!(code_packet(&keys.record_sk, &sealed_theirs, 42).unwrap().timestamp().as_micros(), 42);
    // Garbage in the folder is ignored, never trusted.
    std::fs::write(dir.path().join(format!("{}.pkarr", pk.to_z32())), b"garbage").unwrap();
    assert!(store.candidates(&pk, &readable).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_codes_are_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let a = node_in("alice", Some(dir.path())).await;
    let code = ShortCode::from_bits(u32::from_le_bytes(random_bytes()));
    assert!(matches!(a.net.join(&code.to_string()).await, Err(NetError::Rejected(t)) if t == NO_SUCH_INVITE_CODE));
    assert!(matches!(a.net.resolve_friend_code(code).await, Err(NetError::Rejected(t)) if t == NO_SUCH_FRIEND_CODE));
    // An invite code is not a friend code: separate slot salts, separate records.
    let server = a.net.create_server("S").await.unwrap();
    let ic = a.net.ensure_invite_code(server).await.unwrap().unwrap();
    assert!(a.net.resolve_friend_code(ic).await.is_err());
    a.net.shutdown().await;
}
