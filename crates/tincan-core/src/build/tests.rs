use super::*;
use crate::hlc::hlc_from_ms;
use crate::state::{Change, InviteProblem, Verdict};

const NOW: u64 = 1_800_000_000_000;

fn stored(s: &SignedEvent) -> StoredEvent { StoredEvent::from_signed(s.clone()).unwrap() }

fn apply_ok(state: &mut ConvState, s: &SignedEvent) -> Change {
    let (v, c) = state.apply(&stored(s), crate::now_ms());
    assert_eq!(v, Verdict::Accept);
    c
}

/// A server created by a fresh owner, applied to a fresh state.
fn server() -> (Identity, NewServer, ConvState, Hlc) {
    let owner = Identity::generate();
    let hlc = Hlc::new();
    let ns = new_server(&owner, "  Friends  ", "Ann", &hlc).unwrap();
    let mut state = ConvState::new_server(ns.server);
    for e in &ns.events {
        apply_ok(&mut state, e);
    }
    (owner, ns, state, hlc)
}

#[test]
fn new_server_events_are_valid_and_complete() {
    let (owner, ns, state, _) = server();
    assert_eq!(ns.events.len(), 4);
    let evs: Vec<StoredEvent> = ns.events.iter().map(stored).collect();
    assert!(matches!(evs[0].event.body, Body::Genesis { .. }));
    assert_eq!(evs[0].event.server, ConvId::default());
    assert_eq!(evs[0].conv(), ns.server);
    assert!(evs.windows(2).all(|w| w[0].event.hlc < w[1].event.hlc));
    assert_eq!(evs.iter().map(|e| e.event.seq).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    assert_eq!(state.name, "Friends");
    assert_eq!(state.owner, Some(owner.user()));
    assert_eq!(state.display_name(&owner.user()), "Ann");
    let chans: Vec<(&str, ChannelKind, u32)> = state.sorted_channels().iter().map(|c| (c.name.as_str(), c.kind, c.position)).collect();
    assert_eq!(chans, vec![(DEFAULT_TEXT_CHANNEL, ChannelKind::Text, 0), (DEFAULT_VOICE_CHANNEL, ChannelKind::Voice, 0)]);
    assert_eq!(state.channel(&ns.text_channel).unwrap().kind, ChannelKind::Text);
    assert_eq!(state.channel(&ns.voice_channel).unwrap().kind, ChannelKind::Voice);
    assert_eq!(ns.next_seq, state.next_seq(&owner.user()));
    let keys = ServerKeys::derive(&ns.server_secret, &ns.server);
    assert_eq!(keys.rdv_pk(), ns.keys.rdv_pk());
    assert_eq!(keys.hello_key, ns.keys.hello_key);
    // two servers with the same name differ (random nonce and secret)
    let again = new_server(&owner, "Friends", "Ann", &Hlc::new()).unwrap();
    assert_ne!(again.server, ns.server);
    assert_ne!(again.server_secret, ns.server_secret);
}

#[test]
fn builders_refuse_invalid_input() {
    let owner = Identity::generate();
    let hlc = Hlc::new();
    assert!(matches!(new_server(&owner, "   ", "Ann", &hlc), Err(CoreError::InvalidEvent(_))));
    assert!(new_server(&owner, "S", "", &hlc).is_err());
    assert!(new_server(&owner, &"x".repeat(101), "Ann", &hlc).is_err());
    assert!(profile(&owner, ConvId([1; 32]), 1, 1, "a\nb").is_err());
    assert!(message(&owner, ConvId([1; 32]), 1, 1, crate::EventId([2; 32]), "", vec![]).is_err());
    assert!(message(&owner, ConvId([1; 32]), 1, 1, crate::EventId([2; 32]), &"x".repeat(4001), vec![]).is_err());
}

#[test]
fn event_forces_the_genesis_marker() {
    let owner = Identity::generate();
    let e = event(&owner, ConvId([9; 32]), 1, 1, Body::Genesis { name: "S".into(), owner: owner.user(), nonce: [0; 16] }).unwrap();
    assert_eq!(stored(&e).event.server, ConvId::default());
    let e = event(&owner, ConvId([9; 32]), 1, 1, Body::Profile { name: "p".into() }).unwrap();
    let st = stored(&e);
    assert_eq!(st.event.server, ConvId([9; 32]));
    assert_eq!(st.event.author, owner.user());
    assert_eq!(st.event.v, PROTOCOL_VERSION);
}

#[test]
fn message_delete_leave_builders() {
    let (owner, ns, mut state, hlc) = server();
    let m = message(&owner, ns.server, 5, hlc.now(), ns.text_channel, "hello", vec![]).unwrap();
    let id = m.id();
    assert_eq!(apply_ok(&mut state, &m), Change::Message { id, channel: ns.text_channel });
    let d = delete(&owner, ns.server, 6, hlc.now(), id).unwrap();
    assert_eq!(apply_ok(&mut state, &d), Change::Deleted { target: id });
    let p = profile(&owner, ns.server, 7, hlc.now(), " Annie ").unwrap();
    apply_ok(&mut state, &p);
    assert_eq!(state.display_name(&owner.user()), "Annie");
    let l = leave(&owner, ns.server, 8, hlc.now()).unwrap();
    apply_ok(&mut state, &l);
    assert!(!state.is_member(&owner.user()));
}

#[test]
fn dm_message_builder() {
    let a = Identity::generate();
    let b = Identity::generate();
    let hlc = Hlc::new();
    let mut st = ConvState::new_dm(a.user(), b.user());
    let m = dm_message(&a, &b.user(), 1, hlc.now(), "hey", vec![]).unwrap();
    assert_eq!(stored(&m).event.server, ConvId::dm(&a.user(), &b.user()));
    assert_eq!(apply_ok(&mut st, &m), Change::Message { id: m.id(), channel: st.dm_channel() });
    let r = dm_message(&b, &a.user(), 1, hlc.now(), "hi", vec![]).unwrap();
    apply_ok(&mut st, &r);
    let d = delete(&b, st.conv, 2, hlc.now(), r.id()).unwrap();
    apply_ok(&mut st, &d);
}

#[test]
fn invite_builder() {
    let (owner, ns, mut state, hlc) = server();
    let peers: Vec<(UserId, AddrHint)> = (0..4).map(|i| (UserId([i; 32]), AddrHint::default())).collect();
    let expires = NOW + DEFAULT_INVITE_TTL_MS;
    let (ev, inv) = new_invite(&owner, &state, 5, hlc.now(), expires, peers, &ns.keys, "").unwrap();
    assert_eq!(apply_ok(&mut state, &ev), Change::Invites);
    let Body::InviteCreate { invite_pk, expires_ms, max_uses } = stored(&ev).event.body else { panic!("not an InviteCreate") };
    assert_eq!(invite_pk, inv.invite_pk());
    assert_eq!(expires_ms, expires);
    assert_eq!(max_uses, 0);
    assert_eq!(inv.server, ns.server);
    assert_eq!(inv.name_hint, "Friends");
    assert_eq!(inv.bootstrap.len(), MAX_BOOTSTRAP);
    assert_eq!(inv.rdv_pk, *ns.keys.rdv_pk().as_bytes());
    assert_eq!(inv.rdv_read_key, ns.keys.rdv_read_key);
    assert_eq!(inv.expires_ms, expires);
    assert_eq!(Invite::decode(&inv.encode()).unwrap(), inv);
    // explicit name hint and use limit
    let opts = InviteOptions { expires_ms: expires, max_uses: 3, bootstrap: vec![], name_hint: "Hint".into() };
    let (ev2, inv2) = new_invite_with_limit(&owner, &state, 6, hlc.now(), opts, &ns.keys).unwrap();
    apply_ok(&mut state, &ev2);
    assert_eq!(inv2.name_hint, "Hint");
    assert_eq!(state.invites[&inv2.invite_pk()].max_uses, 3);
    assert_ne!(inv.invite_seed, inv2.invite_seed);
    // revoke
    let r = revoke_invite(&owner, ns.server, 7, hlc.now(), inv2.invite_pk()).unwrap();
    apply_ok(&mut state, &r);
    assert_eq!(state.invite_status(&inv2.invite_pk(), crate::now_ms()), Err(InviteProblem::Revoked));
    // no invites in DMs
    let dm = ConvState::new_dm(owner.user(), UserId([1; 32]));
    assert!(new_invite(&owner, &dm, 1, 1, expires, vec![], &ns.keys, "").is_err());
}

/// Owner creates a server and an invite; returns everything a join test needs.
struct JoinFx {
    host: Identity,
    ns: NewServer,
    state: ConvState,
    hlc: Hlc,
    invite: Invite,
    host_seq: u64,
}

fn join_fx(expires_ms: u64) -> JoinFx {
    let (host, ns, mut state, hlc) = server();
    let (ev, invite) = new_invite(&host, &state, 5, hlc.now(), expires_ms, vec![(host.user(), AddrHint::default())], &ns.keys, "").unwrap();
    apply_ok(&mut state, &ev);
    JoinFx { host, ns, state, hlc, invite, host_seq: 6 }
}

#[test]
fn full_join_flow() {
    let mut fx = join_fx(u64::MAX);
    let joiner = Identity::generate();
    let now = crate::now_ms();
    let jhlc = Hlc::new();
    // joiner decodes the shared code and asks the host
    let invite = Invite::decode(&fx.invite.encode()).unwrap();
    let req = join_request(&joiner, &invite, &fx.host.user(), "Bob", 1, jhlc.now(), now).unwrap();
    let ok = verify_join_request(&req, &fx.state, &fx.host.user(), now).unwrap();
    assert_eq!(ok.joiner, joiner.user());
    assert!(!ok.already_member);
    // host admits, then applies the profile
    let adm = admit(&fx.host, &fx.state, fx.host_seq, fx.hlc.now(), &ok).unwrap();
    assert_eq!(apply_ok(&mut fx.state, &adm), Change::Members);
    assert_eq!(fx.state.apply(&ok.profile, now).0, Verdict::Accept);
    assert_eq!(fx.state.display_name(&joiner.user()), "Bob");
    assert_eq!(fx.state.invites[&invite.invite_pk()].uses, 1);
    // the reply, checked by the joiner
    let reply = join_accepted(&fx.ns.server_secret, &fx.ns.events[0], &adm);
    let acc = verify_join_accepted(&reply, &invite, &joiner.user()).unwrap();
    assert_eq!(acc.server, fx.ns.server);
    assert_eq!(acc.server_secret, fx.ns.server_secret);
    assert_eq!(acc.keys.hello_key, fx.ns.keys.hello_key);
    assert_eq!(acc.admit.id, adm.id());
    // the joiner can now say Hello, and the host can check it
    let info = HelloInfo { epoch_seen: 3, status: Status { synced: true, hosting_ok: true, ..Default::default() }, have: vec![], clips: vec![] };
    let h = hello(&joiner.user(), &acc.server, &acc.keys.hello_key, &fx.host.user(), info.clone());
    assert!(verify_hello(&h, &fx.ns.keys.hello_key, &joiner.user(), &fx.host.user()));
    let Ctl::Hello { epoch_seen, status, .. } = &h else { panic!("not a Hello") };
    assert_eq!((*epoch_seen, status), (3, &info.status));
    // a retried request from a member is recognised
    let again = join_request(&joiner, &invite, &fx.host.user(), "Bob", 1, jhlc.now(), now).unwrap();
    assert!(verify_join_request(&again, &fx.state, &fx.host.user(), now).unwrap().already_member);
}

#[test]
fn removed_member_rejoins_with_a_fresh_admit() {
    let mut fx = join_fx(u64::MAX);
    let joiner = Identity::generate();
    let now = crate::now_ms();
    let jhlc = Hlc::new();
    let req = join_request(&joiner, &fx.invite, &fx.host.user(), "Bob", 1, jhlc.now(), now).unwrap();
    let ok = verify_join_request(&req, &fx.state, &fx.host.user(), now).unwrap();
    let adm = admit(&fx.host, &fx.state, fx.host_seq, fx.hlc.now(), &ok).unwrap();
    apply_ok(&mut fx.state, &adm);
    fx.state.apply(&ok.profile, now);
    // the owner (here also the host) removes Bob at his head
    let rm = remove_member(&fx.host, &fx.state, fx.host_seq + 1, fx.hlc.now(), joiner.user()).unwrap();
    assert_eq!(stored(&rm).event.body, Body::MemberRemove { user: joiner.user(), last_seq: 1 });
    apply_ok(&mut fx.state, &rm);
    assert!(fx.state.is_removed(&joiner.user()));
    // his old admission is not a membership any more: the invite rules apply again, and the
    // profile must continue his chain past the cut
    let stale = join_request(&joiner, &fx.invite, &fx.host.user(), "Bob", 1, jhlc.now(), now).unwrap();
    assert!(verify_join_request(&stale, &fx.state, &fx.host.user(), now).is_err());
    let again = join_request(&joiner, &fx.invite, &fx.host.user(), "Bob", 2, jhlc.now(), now).unwrap();
    let ok = verify_join_request(&again, &fx.state, &fx.host.user(), now).unwrap();
    assert!(!ok.already_member);
    let adm = admit(&fx.host, &fx.state, fx.host_seq + 2, fx.hlc.now(), &ok).unwrap();
    apply_ok(&mut fx.state, &adm);
    assert_eq!(fx.state.apply(&ok.profile, now).0, Verdict::Accept);
    assert!(fx.state.is_member(&joiner.user()));
    // a revoked invite does not let a removed member back in
    let rm = remove_member(&fx.host, &fx.state, fx.host_seq + 3, fx.hlc.now(), joiner.user()).unwrap();
    apply_ok(&mut fx.state, &rm);
    let rv = revoke_invite(&fx.host, fx.ns.server, fx.host_seq + 4, fx.hlc.now(), fx.invite.invite_pk()).unwrap();
    apply_ok(&mut fx.state, &rv);
    let third = join_request(&joiner, &fx.invite, &fx.host.user(), "Bob", 3, jhlc.now(), now).unwrap();
    assert_eq!(verify_join_request(&third, &fx.state, &fx.host.user(), now).unwrap_err().code, RejectCode::BadInvite);
}

#[test]
fn join_request_rejections() {
    let fx = join_fx(u64::MAX);
    let joiner = Identity::generate();
    let now = crate::now_ms();
    let host = fx.host.user();
    let req = |inv: &Invite, to: &UserId, seq: u64, ts: u64| join_request(&joiner, inv, to, "Bob", seq, Hlc::new().now(), ts).unwrap();
    let code = |r: Result<VerifiedJoin, JoinRejection>| r.unwrap_err().code;

    // signed for another host
    let other_host = Identity::generate().user();
    assert_eq!(code(verify_join_request(&req(&fx.invite, &other_host, 1, now), &fx.state, &host, now)), RejectCode::BadInvite);
    // clock skew
    assert_eq!(code(verify_join_request(&req(&fx.invite, &host, 1, now - 10 * 60_000), &fx.state, &host, now)), RejectCode::BadInvite);
    // wrong seq for a first join
    assert_eq!(code(verify_join_request(&req(&fx.invite, &host, 2, now), &fx.state, &host, now)), RejectCode::BadInvite);
    // unknown invite (seed not in any InviteCreate)
    let mut forged = fx.invite.clone();
    forged.invite_seed = [9; 16];
    assert_eq!(code(verify_join_request(&req(&forged, &host, 1, now), &fx.state, &host, now)), RejectCode::BadInvite);
    // another server's state
    let (_, _, other_state, _) = server();
    assert_eq!(code(verify_join_request(&req(&fx.invite, &host, 1, now), &other_state, &host, now)), RejectCode::BadInvite);
    // not a JoinRequest
    assert_eq!(code(verify_join_request(&Ctl::VoiceLeave, &fx.state, &host, now)), RejectCode::Internal);
    // tampered profile
    let Ctl::JoinRequest { server, invite_pk, mut profile, ts_ms, invite_sig } = req(&fx.invite, &host, 1, now) else { panic!() };
    profile.sig.0[0] ^= 1;
    let tampered = Ctl::JoinRequest { server, invite_pk, profile, ts_ms, invite_sig };
    assert_eq!(code(verify_join_request(&tampered, &fx.state, &host, now)), RejectCode::BadInvite);
    // a non-Profile event in place of the profile
    let msg = message(&joiner, fx.ns.server, 1, 1, fx.ns.text_channel, "hi", vec![]).unwrap();
    let wrong = Ctl::JoinRequest { server, invite_pk, profile: msg, ts_ms, invite_sig };
    let rej = verify_join_request(&wrong, &fx.state, &host, now).unwrap_err();
    assert_eq!(rej.code, RejectCode::BadInvite);
    assert!(matches!(rej.to_ctl(), Ctl::Reject { code: RejectCode::BadInvite, .. }));
    assert!(!rej.to_string().is_empty());
}

#[test]
fn join_request_invite_problems() {
    // expired
    let fx = join_fx(NOW);
    let joiner = Identity::generate();
    let host = fx.host.user();
    let now = NOW + 1;
    let r = join_request(&joiner, &fx.invite, &host, "Bob", 1, hlc_from_ms(NOW), now).unwrap();
    assert_eq!(verify_join_request(&r, &fx.state, &host, now).unwrap_err().code, RejectCode::InviteExpired);

    // revoked
    let mut fx = join_fx(u64::MAX);
    let rv = revoke_invite(&fx.host, fx.ns.server, fx.host_seq, fx.hlc.now(), fx.invite.invite_pk()).unwrap();
    apply_ok(&mut fx.state, &rv);
    let now = crate::now_ms();
    let r = join_request(&joiner, &fx.invite, &fx.host.user(), "Bob", 1, Hlc::new().now(), now).unwrap();
    let rej = verify_join_request(&r, &fx.state, &fx.host.user(), now).unwrap_err();
    assert_eq!(rej.code, RejectCode::BadInvite);
    assert_eq!(rej.reason, InviteProblem::Revoked.message());
}

#[test]
fn admit_refuses_an_invite_that_would_fail_validation() {
    let fx = join_fx(NOW);
    let joiner = Identity::generate();
    let now = NOW - 1000;
    let r = join_request(&joiner, &fx.invite, &fx.host.user(), "Bob", 1, hlc_from_ms(now), now).unwrap();
    let ok = verify_join_request(&r, &fx.state, &fx.host.user(), now).unwrap();
    // the host clock (hlc) is past the expiry by the time it authors the admit
    assert!(matches!(admit(&fx.host, &fx.state, fx.host_seq, hlc_from_ms(NOW + 1), &ok), Err(CoreError::InvalidInvite(_))));
    assert!(admit(&fx.host, &fx.state, fx.host_seq, hlc_from_ms(NOW), &ok).is_ok());
    let mut unknown = ok.clone();
    unknown.invite_pk = [1; 32];
    assert!(admit(&fx.host, &fx.state, fx.host_seq, hlc_from_ms(now), &unknown).is_err());
}

#[test]
fn join_accepted_rejections() {
    let mut fx = join_fx(u64::MAX);
    let joiner = Identity::generate();
    let now = crate::now_ms();
    let req = join_request(&joiner, &fx.invite, &fx.host.user(), "Bob", 1, Hlc::new().now(), now).unwrap();
    let ok = verify_join_request(&req, &fx.state, &fx.host.user(), now).unwrap();
    let adm = admit(&fx.host, &fx.state, fx.host_seq, fx.hlc.now(), &ok).unwrap();
    apply_ok(&mut fx.state, &adm);
    let genesis = &fx.ns.events[0];
    // wrong secret
    let bad_secret = join_accepted(&[0; 32], genesis, &adm);
    assert!(verify_join_accepted(&bad_secret, &fx.invite, &joiner.user()).is_err());
    // admission for someone else
    let good = join_accepted(&fx.ns.server_secret, genesis, &adm);
    assert!(verify_join_accepted(&good, &fx.invite, &Identity::generate().user()).is_err());
    // genesis of another server
    let (_, other, _, _) = server();
    let wrong_server = join_accepted(&fx.ns.server_secret, &other.events[0], &adm);
    assert!(verify_join_accepted(&wrong_server, &fx.invite, &joiner.user()).is_err());
    // a non-genesis event as genesis
    let not_genesis = join_accepted(&fx.ns.server_secret, &fx.ns.events[1], &adm);
    assert!(verify_join_accepted(&not_genesis, &fx.invite, &joiner.user()).is_err());
    // not a JoinAccepted
    assert!(verify_join_accepted(&Ctl::VoiceLeave, &fx.invite, &joiner.user()).is_err());
    assert!(verify_join_accepted(&good, &fx.invite, &joiner.user()).is_ok());
}

#[test]
fn hello_mac_checks() {
    let (owner, ns, _, _) = server();
    let member = Identity::generate().user();
    let h = hello(&member, &ns.server, &ns.keys.hello_key, &owner.user(), HelloInfo::default());
    assert!(verify_hello(&h, &ns.keys.hello_key, &member, &owner.user()));
    assert!(!verify_hello(&h, &[0; 32], &member, &owner.user()));
    assert!(!verify_hello(&h, &ns.keys.hello_key, &owner.user(), &member));
    assert!(!verify_hello(&h, &ns.keys.hello_key, &member, &Identity::generate().user()));
    assert!(!verify_hello(&Ctl::VoiceLeave, &ns.keys.hello_key, &member, &owner.user()));
    // nonces are fresh
    let h2 = hello(&member, &ns.server, &ns.keys.hello_key, &owner.user(), HelloInfo::default());
    assert_ne!(h, h2);
}

#[test]
fn host_claim_builder() {
    let id = Identity::generate();
    let server = ServerId::from_bytes([1; 32]);
    let c = host_claim(&id, &server, 4, Some(UserId([2; 32])), 77).unwrap();
    c.verify_for(&server).unwrap();
    assert_eq!((c.claim.epoch, c.claim.host, c.claim.prev, c.claim.issued_ms), (4, id.user(), Some(UserId([2; 32])), 77));
}
