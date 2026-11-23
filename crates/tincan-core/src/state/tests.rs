use std::collections::HashMap;

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;

use super::*;
use crate::BlobHash;
use crate::event::SignedEvent;
use crate::hlc::hlc_from_ms;
use crate::keys::{Identity, Sig64};

/// Wall clock used by every test (ms).
const NOW: u64 = 1_800_000_000_000;
/// Events are timestamped from an hour before NOW.
const T0: u64 = NOW - 3_600_000;
const NEVER: u64 = u64::MAX;

fn raw(id: &Identity, server: ConvId, seq: u64, hlc: u64, body: Body) -> StoredEvent {
    let ev = Event { v: PROTOCOL_VERSION, server, author: id.user(), seq, hlc, body };
    StoredEvent::from_signed(SignedEvent::sign(id, &ev).unwrap()).unwrap()
}

fn text(channel: ChannelId, t: &str) -> Body { Body::Message { channel, text: t.into(), attachments: vec![] } }

fn att(n: &str) -> Attachment { Attachment { hash: BlobHash([5; 32]), size: 10, name: n.into(), mime: "image/png".into() } }

fn named(n: &str) -> Body { Body::Profile { name: n.into() } }

/// A server with an owner, `general` (text), `voice` (voice) and an owner invite that never
/// expires. Tracks seqs per author and a global hlc clock.
struct Fx {
    conv: ConvId,
    state: ConvState,
    owner: Identity,
    general: ChannelId,
    voice: ChannelId,
    invite: [u8; 32],
    seqs: HashMap<UserId, u64>,
    clock_ms: u64,
    log: Vec<StoredEvent>,
}

impl Fx {
    fn new() -> Self {
        let owner = Identity::generate();
        let g = raw(&owner, ConvId::default(), 1, hlc_from_ms(T0), Body::Genesis { name: "Test".into(), owner: owner.user(), nonce: [1; 16] });
        let conv = g.conv();
        let mut state = ConvState::new_server(conv);
        assert_eq!(state.apply(&g, NOW), (Verdict::Accept, Change::ServerRenamed));
        let mut fx = Fx {
            conv,
            state,
            owner: owner.clone(),
            general: EventId::default(),
            voice: EventId::default(),
            invite: [7; 32],
            seqs: HashMap::from([(owner.user(), 1)]),
            clock_ms: T0,
            log: vec![g],
        };
        fx.ok(&owner, named("Owner"));
        fx.general = fx.ok(&owner, Body::ChannelCreate { name: "general".into(), kind: ChannelKind::Text, position: 0 }).id;
        fx.voice = fx.ok(&owner, Body::ChannelCreate { name: "voice".into(), kind: ChannelKind::Voice, position: 0 }).id;
        fx.ok(&owner, Body::InviteCreate { invite_pk: [7; 32], expires_ms: NEVER, max_uses: 0 });
        fx
    }

    /// The next event of `who` (next seq, next second), not applied, seq not consumed.
    fn peek(&mut self, who: &Identity, body: Body) -> StoredEvent {
        self.clock_ms += 1000;
        let seq = self.seqs.get(&who.user()).copied().unwrap_or(0) + 1;
        raw(who, self.conv, seq, hlc_from_ms(self.clock_ms), body)
    }

    /// Apply the next event of `who`; consumes the seq only if accepted.
    fn try_apply(&mut self, who: &Identity, body: Body) -> Verdict {
        let e = self.peek(who, body);
        let (v, _) = self.state.apply(&e, NOW);
        if v.is_accept() {
            self.seqs.insert(who.user(), e.event.seq);
            self.log.push(e);
        }
        v
    }

    /// Apply the next event of `who`, which must be accepted.
    fn ok(&mut self, who: &Identity, body: Body) -> StoredEvent {
        let e = self.peek(who, body);
        let (v, _) = self.state.apply(&e, NOW);
        assert_eq!(v, Verdict::Accept, "{:?}", e.event.body);
        self.seqs.insert(who.user(), e.event.seq);
        self.log.push(e.clone());
        e
    }

    fn next_seq(&self, who: &Identity) -> u64 { self.seqs.get(&who.user()).copied().unwrap_or(0) + 1 }

    /// A new member admitted by the owner with the default invite, with a Profile.
    fn member(&mut self, name: &str) -> Identity {
        let id = Identity::generate();
        let owner = self.owner.clone();
        let from_seq = self.next_seq(&id);
        self.ok(&owner, Body::MemberAdmit { user: id.user(), invite_pk: self.invite, from_seq });
        self.ok(&id, named(name));
        id
    }
}

fn is_reject(v: &Verdict) -> bool { matches!(v, Verdict::Reject(_)) }

// ------------------------------------------------------------------ rule 1: genesis, version, conv

#[test]
fn genesis_is_applied() {
    let fx = Fx::new();
    assert_eq!(fx.state.owner, Some(fx.owner.user()));
    assert_eq!(fx.state.name, "Test");
    assert!(fx.state.is_member(&fx.owner.user()));
    assert!(fx.state.is_owner(&fx.owner.user()));
    assert_eq!(fx.state.members[&fx.owner.user()].admits, vec![1]);
    assert_eq!(fx.state.display_name(&fx.owner.user()), "Owner");
    // the same genesis again is stale
    let g = fx.log[0].clone();
    let mut st = fx.state.clone();
    assert_eq!(st.apply(&g, NOW).0, Verdict::Stale);
}

#[test]
fn genesis_shape_rules() {
    let owner = Identity::generate();
    let body = || Body::Genesis { name: "S".into(), owner: owner.user(), nonce: [0; 16] };

    // server must be [0; 32]: the event's own id is the conv, but it carries a non-zero server
    let e = raw(&owner, ConvId([9; 32]), 1, 1, body());
    let st = ConvState::new_server(ConvId::from_event(&e.id));
    assert!(is_reject(&st.check(&e.id, &e.event, NOW)));

    // id must equal conv
    let e = raw(&owner, ConvId::default(), 1, 1, body());
    let st = ConvState::new_server(ConvId([3; 32]));
    assert!(is_reject(&st.check(&e.id, &e.event, NOW)));

    // seq must be 1
    let e = raw(&owner, ConvId::default(), 2, 1, body());
    let st = ConvState::new_server(e.conv());
    assert!(is_reject(&st.check(&e.id, &e.event, NOW)));

    // author must be the owner named in the body
    let other = Identity::generate();
    let e = raw(&other, ConvId::default(), 1, 1, body());
    let st = ConvState::new_server(e.conv());
    assert!(is_reject(&st.check(&e.id, &e.event, NOW)));

    // name must be valid
    let e = raw(&owner, ConvId::default(), 1, 1, Body::Genesis { name: "  ".into(), owner: owner.user(), nonce: [0; 16] });
    let st = ConvState::new_server(e.conv());
    assert!(is_reject(&st.check(&e.id, &e.event, NOW)));

    // a valid one is accepted, check() does not mutate
    let e = raw(&owner, ConvId::default(), 1, 1, body());
    let st = ConvState::new_server(e.conv());
    assert_eq!(st.check(&e.id, &e.event, NOW), Verdict::Accept);
    assert_eq!(st.owner, None);
    assert!(st.heads.is_empty());

    // genesis is not allowed in a DM
    let st = ConvState::new_dm(owner.user(), other.user());
    assert!(is_reject(&st.check(&e.id, &e.event, NOW)));
}

#[test]
fn events_before_genesis_are_gap() {
    let owner = Identity::generate();
    let g = raw(&owner, ConvId::default(), 1, 1, Body::Genesis { name: "S".into(), owner: owner.user(), nonce: [0; 16] });
    let mut st = ConvState::new_server(g.conv());
    let p = raw(&owner, g.conv(), 2, 2, named("o"));
    assert_eq!(st.apply(&p, NOW).0, Verdict::Gap); // seq gap
    let stranger = Identity::generate();
    let s1 = raw(&stranger, g.conv(), 1, 2, named("s"));
    assert_eq!(st.apply(&s1, NOW).0, Verdict::Gap); // no genesis yet
    assert_eq!(st.apply(&g, NOW).0, Verdict::Accept);
    assert_eq!(st.apply(&p, NOW).0, Verdict::Accept);
    assert_eq!(st.apply(&s1, NOW).0, Verdict::Gap); // not admitted
}

#[test]
fn version_conv_and_seq_zero() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let mut e = fx.peek(&o, named("x")).event;
    e.v = 2;
    assert!(is_reject(&fx.state.check(&EventId([1; 32]), &e, NOW)));
    let mut e = fx.peek(&o, named("x")).event;
    e.server = ConvId([4; 32]);
    assert!(is_reject(&fx.state.check(&EventId([1; 32]), &e, NOW)));
    let mut e = fx.peek(&o, named("x")).event;
    e.server = ConvId::default(); // non-genesis with the genesis marker
    assert!(is_reject(&fx.state.check(&EventId([1; 32]), &e, NOW)));
    let m = fx.member("m");
    let mut e = fx.peek(&m, named("x")).event;
    e.seq = 0;
    assert!(is_reject(&fx.state.check(&EventId([1; 32]), &e, NOW)));
}

// ------------------------------------------------------------------ rules 2 and 3: seq, hlc

#[test]
fn seq_must_be_contiguous() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let next = fx.peek(&o, named("a"));
    let mut skip = next.event.clone();
    skip.seq += 1;
    skip.hlc += 1;
    let skip = StoredEvent::from_signed(SignedEvent::sign(&o, &skip).unwrap()).unwrap();
    assert_eq!(fx.state.apply(&skip, NOW).0, Verdict::Gap);
    assert_eq!(fx.state.apply(&next, NOW).0, Verdict::Accept);
    assert_eq!(fx.state.apply(&next, NOW).0, Verdict::Stale);
    assert_eq!(fx.state.apply(&skip, NOW).0, Verdict::Accept);
    // an older seq with different content is stale too (equivocation keeps the first)
    let mut old = next.event.clone();
    old.body = named("different");
    let old = StoredEvent::from_signed(SignedEvent::sign(&o, &old).unwrap()).unwrap();
    assert_eq!(fx.state.apply(&old, NOW).0, Verdict::Stale);
    assert_eq!(fx.state.head(&o.user()), skip.event.seq);
    assert_eq!(fx.state.next_seq(&o.user()), skip.event.seq + 1);
}

#[test]
fn hlc_must_increase_and_not_be_far_future() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let prev_hlc = fx.state.heads[&o.user()].hlc;
    let seq = fx.next_seq(&o);
    for hlc in [prev_hlc, prev_hlc - 1, 0] {
        let e = raw(&o, fx.conv, seq, hlc, named("x"));
        assert!(is_reject(&fx.state.check(&e.id, &e.event, NOW)), "hlc {hlc}");
    }
    let future = raw(&o, fx.conv, seq, hlc_from_ms(NOW + MAX_FUTURE_MS + 1), named("x"));
    assert_eq!(fx.state.apply(&future, NOW).0, Verdict::Future);
    assert_eq!(fx.state.apply(&future, NOW + 1).0, Verdict::Accept);
    let edge = raw(&o, fx.conv, seq + 1, hlc_from_ms(NOW + MAX_FUTURE_MS + 1) + 1, named("y"));
    assert_eq!(fx.state.apply(&edge, NOW + 1).0, Verdict::Accept);
}

#[test]
fn malformed_events_are_rejected_even_at_a_gap() {
    let mut fx = Fx::new();
    let m = fx.member("m");
    let seq = fx.next_seq(&m) + 3;
    let e = raw(&m, fx.conv, seq, hlc_from_ms(fx.clock_ms + 1000), text(fx.general, ""));
    assert!(is_reject(&fx.state.check(&e.id, &e.event, NOW)));
    let e = raw(&m, fx.conv, seq, hlc_from_ms(fx.clock_ms + 1000), Body::MemberLeave { last_seq: 1 });
    assert!(is_reject(&fx.state.check(&e.id, &e.event, NOW)));
    let e = raw(&m, fx.conv, seq, hlc_from_ms(fx.clock_ms + 1000), text(fx.general, "fine"));
    assert_eq!(fx.state.check(&e.id, &e.event, NOW), Verdict::Gap);
    // a DM non-party at a gap position
    let a = Identity::generate();
    let b = Identity::generate();
    let dm = ConvState::new_dm(a.user(), b.user());
    let e = raw(&m, dm.conv, 5, 1, text(dm.dm_channel(), "x"));
    assert!(is_reject(&dm.check(&e.id, &e.event, NOW)));
    let e = raw(&a, dm.conv, 5, 1, text(dm.dm_channel(), "x"));
    assert_eq!(dm.check(&e.id, &e.event, NOW), Verdict::Gap);
}

// ------------------------------------------------------------------ rule 6: membership

#[test]
fn membership_needs_an_admit() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let joiner = Identity::generate();
    assert_eq!(fx.try_apply(&joiner, named("j")), Verdict::Gap);
    assert!(!fx.state.is_member(&joiner.user()));
    fx.ok(&o, Body::MemberAdmit { user: joiner.user(), invite_pk: fx.invite, from_seq: 1 });
    assert!(fx.state.is_member(&joiner.user()));
    fx.ok(&joiner, named("j"));
    assert_eq!(fx.state.display_name(&joiner.user()), "j");
    assert_eq!(fx.state.invites[&fx.invite].uses, 1);
    // a stranger's name falls back to the z32 prefix
    let s = Identity::generate().user();
    assert_eq!(fx.state.display_name(&s), s.to_z32().chars().take(8).collect::<String>());
}

#[test]
fn leave_and_readmit() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let m = fx.member("m"); // seq 1
    fx.ok(&m, text(fx.general, "hi")); // seq 2
    // bad last_seq
    assert!(is_reject(&fx.try_apply(&m, Body::MemberLeave { last_seq: 1 })));
    assert!(is_reject(&fx.try_apply(&m, Body::MemberLeave { last_seq: 4 })));
    fx.ok(&m, Body::MemberLeave { last_seq: 3 }); // seq 3, last
    assert!(!fx.state.is_member(&m.user()));
    assert_eq!(fx.state.members[&m.user()].left_at, Some(3));
    assert_eq!(fx.try_apply(&m, text(fx.general, "after")), Verdict::Gap);
    // a stale duplicate admission (from_seq 1) does not re-admit
    fx.ok(&o, Body::MemberAdmit { user: m.user(), invite_pk: fx.invite, from_seq: 1 });
    assert!(!fx.state.is_member(&m.user()));
    assert_eq!(fx.try_apply(&m, text(fx.general, "after")), Verdict::Gap);
    // a real re-admission does
    fx.ok(&o, Body::MemberAdmit { user: m.user(), invite_pk: fx.invite, from_seq: 4 });
    assert!(fx.state.is_member(&m.user()));
    assert_eq!(fx.state.members[&m.user()].admitted_from_seq, 4);
    assert_eq!(fx.state.members[&m.user()].admits, vec![1, 4]);
    fx.ok(&m, text(fx.general, "back")); // seq 4
    // leave with last_seq = seq - 1: the leave itself is accepted, later events are not
    fx.ok(&m, Body::MemberLeave { last_seq: 4 }); // seq 5
    assert_eq!(fx.state.members[&m.user()].left_at, Some(4));
    assert_eq!(fx.try_apply(&m, named("x")), Verdict::Gap);
}

#[test]
fn readmit_known_before_the_leave() {
    // the host saw the leave and re-admitted; we get the re-admit first
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let m = fx.member("m"); // seq 1
    fx.ok(&o, Body::MemberAdmit { user: m.user(), invite_pk: fx.invite, from_seq: 3 });
    assert!(fx.state.is_member(&m.user()));
    fx.ok(&m, Body::MemberLeave { last_seq: 2 }); // seq 2
    assert!(fx.state.is_member(&m.user()), "re-admission from 3 is after the leave");
    fx.ok(&m, named("back")); // seq 3
}

#[test]
fn duplicate_admit_of_a_member_is_a_noop() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let m = fx.member("m");
    let before = fx.state.members[&m.user()].clone();
    fx.ok(&o, Body::MemberAdmit { user: m.user(), invite_pk: fx.invite, from_seq: 1 });
    assert_eq!(fx.state.members[&m.user()], before);
    assert!(is_reject(&fx.try_apply(&o, Body::MemberAdmit { user: m.user(), invite_pk: fx.invite, from_seq: 0 })));
}

#[test]
fn owner_can_leave_and_loses_authority() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let seq = fx.next_seq(&o);
    fx.ok(&o, Body::MemberLeave { last_seq: seq });
    assert!(!fx.state.is_member(&o.user()));
    assert_eq!(fx.try_apply(&o, Body::ServerUpdate { name: Some("x".into()) }), Verdict::Gap);
}

// ------------------------------------------------------------------ owner-only rules

#[test]
fn owner_only_rules() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let m = fx.member("m");
    assert!(is_reject(&fx.try_apply(&m, Body::ChannelCreate { name: "x".into(), kind: ChannelKind::Text, position: 0 })));
    assert!(is_reject(&fx.try_apply(&m, Body::ChannelUpdate { channel: fx.general, name: Some("x".into()), position: None, deleted: None })));
    assert!(is_reject(&fx.try_apply(&m, Body::ServerUpdate { name: Some("x".into()) })));
    // a stranger's owner-only event is rejected outright, not buffered
    let s = Identity::generate();
    assert!(is_reject(&fx.try_apply(&s, Body::ServerUpdate { name: Some("x".into()) })));

    let e = fx.peek(&o, Body::ServerUpdate { name: Some("Renamed".into()) });
    assert_eq!(fx.state.apply(&e, NOW), (Verdict::Accept, Change::ServerRenamed));
    fx.seqs.insert(o.user(), e.event.seq);
    assert_eq!(fx.state.name, "Renamed");
    let e = fx.peek(&o, Body::ServerUpdate { name: None });
    assert_eq!(fx.state.apply(&e, NOW), (Verdict::Accept, Change::Nothing));
    fx.seqs.insert(o.user(), e.event.seq);

    let e = fx.peek(&o, Body::ChannelUpdate { channel: fx.general, name: Some("lobby".into()), position: Some(3), deleted: None });
    assert_eq!(fx.state.apply(&e, NOW), (Verdict::Accept, Change::Channels));
    fx.seqs.insert(o.user(), e.event.seq);
    let ch = fx.state.channel(&fx.general).unwrap();
    assert_eq!((ch.name.as_str(), ch.position, ch.deleted), ("lobby", 3, false));
    // update of an unknown channel by the owner can never become valid
    assert!(is_reject(&fx.try_apply(&o, Body::ChannelUpdate { channel: EventId([1; 32]), name: None, position: None, deleted: Some(true) })));
    // invalid names
    assert!(is_reject(&fx.try_apply(&o, Body::ChannelCreate { name: " ".into(), kind: ChannelKind::Text, position: 0 })));
    assert!(is_reject(&fx.try_apply(&o, Body::ChannelUpdate { channel: fx.general, name: Some("a\u{7}".into()), position: None, deleted: None })));
    assert!(is_reject(&fx.try_apply(&o, Body::ServerUpdate { name: Some(String::new()) })));
}

#[test]
fn channels_sort_and_delete() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let b = fx.ok(&o, Body::ChannelCreate { name: "b".into(), kind: ChannelKind::Text, position: 1 }).id;
    let a = fx.ok(&o, Body::ChannelCreate { name: "a".into(), kind: ChannelKind::Text, position: 1 }).id;
    let v2 = fx.ok(&o, Body::ChannelCreate { name: "v2".into(), kind: ChannelKind::Voice, position: 0 }).id;
    let names: Vec<&str> = fx.state.sorted_channels().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["general", "a", "b", "v2", "voice"]);
    fx.ok(&o, Body::ChannelUpdate { channel: b, name: None, position: None, deleted: Some(true) });
    let names: Vec<&str> = fx.state.sorted_channels().iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["general", "a", "v2", "voice"]);
    assert!(fx.state.channel(&b).unwrap().deleted);
    // restore
    fx.ok(&o, Body::ChannelUpdate { channel: b, name: None, position: None, deleted: Some(false) });
    assert_eq!(fx.state.sorted_channels().len(), 5);
    let _ = (a, v2);
}

// ------------------------------------------------------------------ messages

#[test]
fn message_rules() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let m = fx.member("m");
    let e = fx.peek(&m, text(fx.general, "hello"));
    assert_eq!(fx.state.apply(&e, NOW), (Verdict::Accept, Change::Message { id: e.id, channel: fx.general }));
    fx.seqs.insert(m.user(), e.event.seq);
    assert_eq!(fx.state.message_authors[&e.id], m.user());

    assert!(is_reject(&fx.try_apply(&m, text(fx.voice, "in voice"))));
    // unknown channel: another author's ChannelCreate may still arrive
    assert_eq!(fx.try_apply(&m, text(EventId([1; 32]), "where")), Verdict::Gap);
    // ...but the owner's own channels are always known first
    assert!(is_reject(&fx.try_apply(&o, text(EventId([1; 32]), "where"))));

    // text length in chars, not bytes
    assert_eq!(fx.try_apply(&m, text(fx.general, &"x".repeat(MAX_TEXT_CHARS))), Verdict::Accept);
    assert_eq!(fx.try_apply(&m, text(fx.general, &"é".repeat(MAX_TEXT_CHARS))), Verdict::Accept);
    assert!(is_reject(&fx.try_apply(&m, text(fx.general, &"x".repeat(MAX_TEXT_CHARS + 1)))));
    // attachments
    let ten: Vec<Attachment> = (0..MAX_ATTACHMENTS).map(|i| att(&format!("f{i}.png"))).collect();
    assert_eq!(fx.try_apply(&m, Body::Message { channel: fx.general, text: String::new(), attachments: ten.clone() }), Verdict::Accept);
    let mut eleven = ten;
    eleven.push(att("one-too-many"));
    assert!(is_reject(&fx.try_apply(&m, Body::Message { channel: fx.general, text: "x".into(), attachments: eleven })));
    assert!(is_reject(&fx.try_apply(&m, Body::Message { channel: fx.general, text: "x".into(), attachments: vec![att("")] })));
    let mut long_mime = att("a");
    long_mime.mime = "x".repeat(MAX_MIME_CHARS + 1);
    assert!(is_reject(&fx.try_apply(&m, Body::Message { channel: fx.general, text: "x".into(), attachments: vec![long_mime] })));
    // empty message
    assert!(is_reject(&fx.try_apply(&m, text(fx.general, ""))));
    assert!(fx.state.can_post(&m.user(), &fx.general));
    assert!(!fx.state.can_post(&m.user(), &fx.voice));
    assert!(!fx.state.can_post(&Identity::generate().user(), &fx.general));
}

#[test]
fn message_to_a_deleted_channel_is_accepted_but_not_postable() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let m = fx.member("m");
    fx.ok(&o, Body::ChannelUpdate { channel: fx.general, name: None, position: None, deleted: Some(true) });
    assert!(!fx.state.can_post(&m.user(), &fx.general));
    // a member that had not seen the deletion yet: accepted, so every replica converges
    assert_eq!(fx.try_apply(&m, text(fx.general, "concurrent")), Verdict::Accept);
}

// ------------------------------------------------------------------ deletes

#[test]
fn delete_authority() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let a = fx.member("a");
    let b = fx.member("b");
    let ma = fx.ok(&a, text(fx.general, "a1")).id;
    let ma2 = fx.ok(&a, text(fx.general, "a2")).id;
    let mo = fx.ok(&o, text(fx.general, "o1")).id;
    // someone else's message
    assert!(is_reject(&fx.try_apply(&b, Body::Delete { target: ma })));
    assert!(is_reject(&fx.try_apply(&a, Body::Delete { target: mo })));
    // own message
    let e = fx.peek(&a, Body::Delete { target: ma });
    assert_eq!(fx.state.apply(&e, NOW), (Verdict::Accept, Change::Deleted { target: ma }));
    fx.seqs.insert(a.user(), e.event.seq);
    assert!(fx.state.deleted.contains(&ma));
    // owner deletes anyone's; deleting twice is fine
    assert_eq!(fx.try_apply(&o, Body::Delete { target: ma2 }), Verdict::Accept);
    assert_eq!(fx.try_apply(&o, Body::Delete { target: ma }), Verdict::Accept);
    // unknown target: only the owner can be waiting for another author's message
    assert!(is_reject(&fx.try_apply(&a, Body::Delete { target: EventId([3; 32]) })));
    assert_eq!(fx.try_apply(&o, Body::Delete { target: EventId([3; 32]) }), Verdict::Gap);
    // non-message events cannot be deleted (never known as messages)
    let profile_id = fx.log[1].id;
    assert!(is_reject(&fx.try_apply(&a, Body::Delete { target: profile_id })));
}

// ------------------------------------------------------------------ invites

#[test]
fn invite_create_and_revoke() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let a = fx.member("a");
    let b = fx.member("b");
    let pk = [42; 32];
    let e = fx.peek(&a, Body::InviteCreate { invite_pk: pk, expires_ms: NOW + 1000, max_uses: 1 });
    assert_eq!(fx.state.apply(&e, NOW), (Verdict::Accept, Change::Invites));
    fx.seqs.insert(a.user(), e.event.seq);
    assert_eq!(fx.state.invites[&pk].creator, a.user());
    assert!(is_reject(&fx.try_apply(&b, Body::InviteCreate { invite_pk: pk, expires_ms: NEVER, max_uses: 0 })));
    assert!(is_reject(&fx.try_apply(&b, Body::InviteRevoke { invite_pk: pk })));
    assert_eq!(fx.try_apply(&a, Body::InviteRevoke { invite_pk: pk }), Verdict::Accept);
    assert!(fx.state.invites[&pk].revoked);
    assert_eq!(fx.try_apply(&a, Body::InviteRevoke { invite_pk: pk }), Verdict::Accept); // idempotent
    // the owner may revoke anyone's
    let pk2 = [43; 32];
    fx.ok(&b, Body::InviteCreate { invite_pk: pk2, expires_ms: NEVER, max_uses: 0 });
    assert_eq!(fx.try_apply(&o, Body::InviteRevoke { invite_pk: pk2 }), Verdict::Accept);
    // unknown invite: a member can only revoke its own (known), the owner may wait
    assert!(is_reject(&fx.try_apply(&a, Body::InviteRevoke { invite_pk: [44; 32] })));
    assert_eq!(fx.try_apply(&o, Body::InviteRevoke { invite_pk: [44; 32] }), Verdict::Gap);
}

#[test]
fn admit_invite_rules_and_status() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let host = fx.member("host");
    // unknown invite: its creator's event may still arrive
    let j = Identity::generate();
    assert_eq!(fx.try_apply(&host, Body::MemberAdmit { user: j.user(), invite_pk: [50; 32], from_seq: 1 }), Verdict::Gap);
    assert_eq!(fx.state.invite_status(&[50; 32], NOW), Err(InviteProblem::Unknown));

    // expiry is judged at the admit's hlc, deterministically
    let pk = [51; 32];
    let expires = fx.clock_ms + 5_000;
    fx.ok(&o, Body::InviteCreate { invite_pk: pk, expires_ms: expires, max_uses: 2 });
    assert!(fx.state.invite_status(&pk, expires).is_ok());
    assert_eq!(fx.state.invite_status(&pk, expires + 1), Err(InviteProblem::Expired));
    let j1 = Identity::generate();
    fx.ok(&host, Body::MemberAdmit { user: j1.user(), invite_pk: pk, from_seq: 1 }); // within 5 s
    fx.clock_ms = expires + 10_000;
    let j2 = Identity::generate();
    assert!(is_reject(&fx.try_apply(&host, Body::MemberAdmit { user: j2.user(), invite_pk: pk, from_seq: 1 })));

    // max_uses and revocation are host policy (invite_status), not event validation
    let pk3 = [52; 32];
    fx.ok(&o, Body::InviteCreate { invite_pk: pk3, expires_ms: NEVER, max_uses: 1 });
    let j3 = Identity::generate();
    fx.ok(&host, Body::MemberAdmit { user: j3.user(), invite_pk: pk3, from_seq: 1 });
    assert_eq!(fx.state.invite_status(&pk3, NOW), Err(InviteProblem::UsedUp));
    let j4 = Identity::generate();
    assert_eq!(fx.try_apply(&host, Body::MemberAdmit { user: j4.user(), invite_pk: pk3, from_seq: 1 }), Verdict::Accept);
    assert_eq!(fx.state.invites[&pk3].uses, 2);
    fx.ok(&o, Body::InviteRevoke { invite_pk: pk3 });
    assert_eq!(fx.state.invite_status(&pk3, NOW), Err(InviteProblem::Revoked));
    let j5 = Identity::generate();
    assert_eq!(fx.try_apply(&host, Body::MemberAdmit { user: j5.user(), invite_pk: pk3, from_seq: 1 }), Verdict::Accept);
    // unlimited invite
    assert!(fx.state.invite_status(&fx.invite, NOW).is_ok());
    assert_eq!(InviteProblem::Expired.reject_code(), RejectCode::InviteExpired);
    assert_eq!(InviteProblem::Revoked.reject_code(), RejectCode::BadInvite);
    assert!(!InviteProblem::UsedUp.message().is_empty());
}

// ------------------------------------------------------------------ names

#[test]
fn name_rules() {
    let mut fx = Fx::new();
    let m = fx.member("m");
    for bad in ["", "   ", "a\nb", "tab\there"] {
        assert!(is_reject(&fx.try_apply(&m, named(bad))), "{bad:?}");
    }
    assert!(is_reject(&fx.try_apply(&m, named(&"x".repeat(MAX_NAME_CHARS + 1)))));
    assert_eq!(fx.try_apply(&m, named(&"ü".repeat(MAX_NAME_CHARS))), Verdict::Accept);
    assert_eq!(fx.try_apply(&m, named("Zoë 🎧")), Verdict::Accept);
    assert_eq!(fx.state.display_name(&m.user()), "Zoë 🎧");
    assert!(validate_name("ok").is_ok());
    assert!(validate_name("").is_err());
    assert!(validate_message("hi", &[]).is_ok());
    assert!(validate_message("", &[]).is_err());
}

// ------------------------------------------------------------------ DMs

#[test]
fn dm_rules() {
    let a = Identity::generate();
    let b = Identity::generate();
    let c = Identity::generate();
    let mut st = ConvState::new_dm(a.user(), b.user());
    assert_eq!(st.conv, ConvId::dm(&b.user(), &a.user()));
    let ch = st.dm_channel();
    let conv = st.conv;
    let m1 = raw(&a, conv, 1, 10, text(ch, "hi b"));
    assert_eq!(st.apply(&m1, NOW), (Verdict::Accept, Change::Message { id: m1.id, channel: ch }));
    let m2 = raw(&b, conv, 1, 11, text(ch, "hi a"));
    assert_eq!(st.apply(&m2, NOW).0, Verdict::Accept);
    // wrong channel, non-party, non-message bodies
    assert!(is_reject(&st.check(&EventId([1; 32]), &raw(&a, conv, 2, 12, text(EventId([9; 32]), "x")).event, NOW)));
    let cm = raw(&c, conv, 1, 12, text(ch, "intruder"));
    assert!(is_reject(&st.apply(&cm, NOW).0));
    for body in [
        named("a"),
        Body::InviteCreate { invite_pk: [1; 32], expires_ms: NEVER, max_uses: 0 },
        Body::MemberLeave { last_seq: 2 },
        Body::ServerUpdate { name: Some("x".into()) },
    ] {
        let e = raw(&a, conv, 2, 12, body);
        assert!(is_reject(&st.check(&e.id, &e.event, NOW)));
    }
    // deletes: own only, target must be known
    let d_other = raw(&a, conv, 2, 13, Body::Delete { target: m2.id });
    assert!(is_reject(&st.apply(&d_other, NOW).0));
    let d_unknown = raw(&a, conv, 2, 13, Body::Delete { target: EventId([8; 32]) });
    assert!(is_reject(&st.apply(&d_unknown, NOW).0));
    let d_own = raw(&a, conv, 2, 13, Body::Delete { target: m1.id });
    assert_eq!(st.apply(&d_own, NOW), (Verdict::Accept, Change::Deleted { target: m1.id }));
    assert!(st.is_member(&a.user()) && st.is_member(&b.user()) && !st.is_member(&c.user()));
    assert!(st.can_post(&a.user(), &ch));
    assert!(!st.can_post(&a.user(), &EventId([9; 32])));
    assert!(!st.can_post(&c.user(), &ch));
    // events of the DM are not valid in a server state and vice versa
    let server = ConvState::new_server(ConvId([1; 32]));
    assert!(is_reject(&server.check(&m1.id, &m1.event, NOW)));
}

// ------------------------------------------------------------------ sync helpers

#[test]
fn sync_heads_and_author_order() {
    let mut fx = Fx::new();
    let a = fx.member("a");
    let b = fx.member("b");
    fx.ok(&a, text(fx.general, "x"));
    let heads = fx.state.sync_heads();
    assert!(heads.0.windows(2).all(|w| w[0].0 < w[1].0));
    let map = heads.to_map();
    assert_eq!(map[&fx.owner.user()], fx.next_seq(&fx.owner) - 1);
    assert_eq!(map[&a.user()], 2);
    assert_eq!(map[&b.user()], 1);
    assert_eq!(fx.state.sync_author_order(), vec![fx.owner.user(), a.user(), b.user()]);
}

// ------------------------------------------------------------------ convergence

/// A history with the order-sensitive cases: a message concurrent with its channel's
/// deletion, an admission concurrent with its invite's revocation, a re-admission, deletes,
/// renames. Returns the events in a valid order and the state they produce.
fn scenario() -> (ConvId, Vec<StoredEvent>, ConvState) {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let host = Identity::generate();
    let a = Identity::generate();
    let b = Identity::generate();
    let c = Identity::generate();
    fx.ok(&o, Body::MemberAdmit { user: host.user(), invite_pk: fx.invite, from_seq: 1 });
    fx.ok(&host, named("host"));
    let inv2 = [2; 32];
    fx.ok(&host, Body::InviteCreate { invite_pk: inv2, expires_ms: NEVER, max_uses: 0 });
    fx.ok(&host, Body::MemberAdmit { user: a.user(), invite_pk: inv2, from_seq: 1 });
    fx.ok(&a, named("a"));
    fx.ok(&host, Body::MemberAdmit { user: b.user(), invite_pk: inv2, from_seq: 1 });
    fx.ok(&b, named("b"));
    let random = fx.ok(&o, Body::ChannelCreate { name: "random".into(), kind: ChannelKind::Text, position: 1 }).id;
    let m1 = fx.ok(&a, text(fx.general, "hello")).id;
    let m2 = fx.ok(&b, text(fx.general, "hi")).id;
    fx.ok(&a, text(random, "in random"));
    fx.ok(&o, Body::ChannelUpdate { channel: random, name: None, position: None, deleted: Some(true) });
    fx.ok(&a, text(random, "posted before seeing the delete"));
    fx.ok(&o, Body::InviteRevoke { invite_pk: inv2 });
    fx.ok(&host, Body::MemberAdmit { user: c.user(), invite_pk: inv2, from_seq: 1 });
    fx.ok(&c, named("c"));
    fx.ok(&c, text(fx.general, "c here"));
    fx.ok(&b, Body::MemberLeave { last_seq: 3 });
    fx.ok(&host, Body::MemberAdmit { user: b.user(), invite_pk: fx.invite, from_seq: 4 });
    fx.ok(&b, named("b again"));
    fx.ok(&b, text(fx.general, "back"));
    fx.ok(&a, Body::Delete { target: m1 });
    fx.ok(&o, Body::Delete { target: m2 });
    fx.ok(&a, Body::InviteCreate { invite_pk: [3; 32], expires_ms: NEVER, max_uses: 0 });
    fx.ok(&a, Body::InviteRevoke { invite_pk: [3; 32] });
    fx.ok(&o, Body::ServerUpdate { name: Some("Renamed".into()) });
    fx.ok(&o, Body::ChannelUpdate { channel: fx.general, name: Some("lobby".into()), position: Some(2), deleted: None });
    fx.ok(&host, named("host 2"));

    // sanity of the reference state
    let st = &fx.state;
    for u in [host.user(), a.user(), b.user(), c.user()] {
        assert!(st.is_member(&u));
    }
    assert_eq!(st.members[&b.user()].admits, vec![1, 4]);
    assert_eq!(st.members[&b.user()].last_leave, Some(3));
    assert_eq!(st.display_name(&b.user()), "b again");
    assert!(st.channels[&random].deleted);
    assert!(st.invites[&inv2].revoked);
    assert_eq!(st.invites[&inv2].uses, 3);
    assert_eq!(st.invites[&fx.invite].uses, 2);
    assert_eq!(st.name, "Renamed");
    assert!(st.deleted.contains(&m1) && st.deleted.contains(&m2));
    (fx.conv, fx.log.clone(), fx.state.clone())
}

fn drain_all(conv: ConvId, events: Vec<StoredEvent>) -> (ConvState, Vec<StoredEvent>, PendingBuffer) {
    let mut st = ConvState::new_server(conv);
    let mut pending = PendingBuffer::new();
    let mut applied = Vec::new();
    for e in events {
        let (_, out) = pending.ingest(&mut st, e, NOW);
        applied.extend(out.into_iter().map(|(e, _)| e));
    }
    (st, applied, pending)
}

#[test]
fn shuffled_delivery_converges() {
    let (conv, events, reference) = scenario();
    for seed in 0..40u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut evs = events.clone();
        evs.shuffle(&mut rng);
        let (st, applied, pending) = drain_all(conv, evs);
        assert!(pending.is_empty(), "seed {seed}: {} left pending", pending.len());
        assert_eq!(applied.len(), events.len(), "seed {seed}");
        assert_eq!(st, reference, "seed {seed}");
        // the applied order is a valid dependency order for plain `apply`
        let mut replay = ConvState::new_server(conv);
        for e in &applied {
            assert_eq!(replay.apply(e, NOW).0, Verdict::Accept, "seed {seed}");
        }
        assert_eq!(replay, reference);
    }
}

#[test]
fn worst_case_orders_converge() {
    let (conv, events, reference) = scenario();
    // fully reversed
    let mut rev = events.clone();
    rev.reverse();
    assert_eq!(drain_all(conv, rev).0, reference);
    // grouped by author, highest seq first, authors in reverse id order
    let mut grouped = events.clone();
    grouped.sort_by_key(|e| std::cmp::Reverse((e.event.author, e.event.seq)));
    assert_eq!(drain_all(conv, grouped).0, reference);
    // duplicates delivered many times
    let mut dup = events.clone();
    dup.extend(events.iter().cloned());
    dup.reverse();
    dup.extend(events.iter().cloned());
    let (st, applied, pending) = drain_all(conv, dup);
    assert_eq!(st, reference);
    assert_eq!(applied.len(), events.len());
    assert!(pending.is_empty());
}

#[test]
fn rebuild_is_deterministic_and_matches_live() {
    let (conv, events, reference) = scenario();
    for seed in 0..20u64 {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut evs = events.clone();
        evs.shuffle(&mut rng);
        if seed % 2 == 0 {
            evs.extend(events.iter().take(5).cloned()); // duplicates are ignored
        }
        assert_eq!(ConvState::rebuild(conv, None, evs, NOW), reference, "seed {seed}");
    }
    // a clock that went backwards since the events were accepted does not hold them back
    assert_eq!(ConvState::rebuild(conv, None, events.clone(), 0), reference);
    // empty input
    let empty = ConvState::rebuild(conv, None, Vec::new(), NOW);
    assert_eq!(empty, ConvState::new_server(conv));
}

#[test]
fn rebuild_from_partial_input() {
    let (conv, events, reference) = scenario();
    // state events only (no messages): holes are tolerated, deletes of unknown messages are
    // applied as trusted, membership and invites are intact
    let state_only: Vec<StoredEvent> = events.iter().filter(|e| !matches!(e.event.body, Body::Message { .. })).cloned().collect();
    let st = ConvState::rebuild(conv, None, state_only, NOW);
    assert_eq!(st.members, reference.members);
    assert_eq!(st.invites, reference.invites);
    assert_eq!(st.channels, reference.channels);
    assert_eq!(st.deleted, reference.deleted);
    assert_eq!(st.name, reference.name);
    assert!(st.message_authors.is_empty());
    // heads are as high as the input allows (short where trailing messages are missing)
    for (u, h) in &st.heads {
        assert!(h.seq <= reference.heads[u].seq);
    }
    // a missing admission does not lose the member's events
    let no_admits: Vec<StoredEvent> = events.iter().filter(|e| !matches!(e.event.body, Body::MemberAdmit { .. })).cloned().collect();
    let st = ConvState::rebuild(conv, None, no_admits, NOW);
    assert_eq!(st.heads.len(), reference.heads.len());
    assert_eq!(st.message_authors, reference.message_authors);
    // events of a different conversation are dropped
    let mut other = events.clone();
    let stranger = Identity::generate();
    other.push(raw(&stranger, ConvId([5; 32]), 1, 1, named("elsewhere")));
    assert_eq!(ConvState::rebuild(conv, None, other, NOW), reference);
}

#[test]
fn rebuild_dm() {
    let a = Identity::generate();
    let b = Identity::generate();
    let mut st = ConvState::new_dm(a.user(), b.user());
    let ch = st.dm_channel();
    let conv = st.conv;
    let mut events = Vec::new();
    for i in 1..=5u64 {
        let who = if i % 2 == 0 { &a } else { &b };
        let seq = i.div_ceil(2);
        let e = raw(who, conv, seq, i * 10, text(ch, &format!("m{i}")));
        assert_eq!(st.apply(&e, NOW).0, Verdict::Accept);
        events.push(e);
    }
    let target = events[0].id; // b's first message
    let d = raw(&b, conv, 4, 100, Body::Delete { target });
    assert_eq!(st.apply(&d, NOW).0, Verdict::Accept);
    events.push(d);
    events.reverse();
    assert_eq!(ConvState::rebuild(conv, Some((a.user(), b.user())), events.clone(), NOW), st);
    assert_eq!(ConvState::rebuild(conv, Some((b.user(), a.user())), events, NOW).conv, conv);
}

// ------------------------------------------------------------------ pending buffer

fn fake(author: UserId, seq: u64, name_len: usize) -> StoredEvent {
    let event = Event { v: PROTOCOL_VERSION, server: ConvId([1; 32]), author, seq, hlc: seq, body: Body::Profile { name: "x".repeat(name_len) } };
    let bytes = postcard::to_stdvec(&event).unwrap();
    let signed = SignedEvent { bytes, sig: Sig64::ZERO };
    StoredEvent { id: signed.id(), signed, event, deleted: false }
}

#[test]
fn pending_limits_keep_junk_from_evicting_honest_early_events() {
    let mut fx = Fx::new();
    let m = fx.member("m");
    // An honest early event: m's next-but-one event (its predecessor is still on the way).
    let seq = fx.next_seq(&m);
    let honest = raw(&m, fx.conv, seq + 1, hlc_from_ms(fx.clock_ms + 5000), named("early"));
    let mut pending = PendingBuffer::with_limits(PendingLimits::network());
    assert_eq!(pending.ingest(&mut fx.state, honest.clone(), NOW).0, Verdict::Gap);
    // A flood of validly signed events from fresh non-member keys (each a Gap).
    for _ in 0..(MAX_PENDING_EVENTS + 50) {
        let junk = Identity::generate();
        let e = raw(&junk, fx.conv, 1, hlc_from_ms(T0), named("junk"));
        assert_eq!(pending.ingest(&mut fx.state, e, NOW).0, Verdict::Gap);
    }
    assert!(pending.contains(&m.user(), seq + 1), "the honest early event survived the flood");
    assert!(pending.len() <= 1 + PendingLimits::network().non_member_events);
    // One author far ahead of its head, or far in the future, is not buffered.
    let far = raw(&m, fx.conv, seq + 10_000, hlc_from_ms(fx.clock_ms + 6000), named("far"));
    pending.ingest(&mut fx.state, far, NOW);
    assert!(!pending.contains(&m.user(), seq + 10_000));
    let future = raw(&m, fx.conv, seq, hlc_from_ms(NOW + 2 * 3_600_000), named("future"));
    assert_eq!(pending.ingest(&mut fx.state, future, NOW).0, Verdict::Future);
    assert!(!pending.contains(&m.user(), seq));
    let future_gap = raw(&m, fx.conv, seq + 2, hlc_from_ms(NOW + 2 * 3_600_000), named("future gap"));
    pending.ingest(&mut fx.state, future_gap, NOW);
    assert!(!pending.contains(&m.user(), seq + 2));
    // A per-author quota: one member's run cannot fill the buffer either.
    let limits = PendingLimits { per_author_events: 3, ..PendingLimits::network() };
    let mut small = PendingBuffer::with_limits(limits);
    for i in 1..=10 {
        small.ingest(&mut fx.state, raw(&m, fx.conv, seq + i, hlc_from_ms(fx.clock_ms + 7000 + i), named("run")), NOW);
    }
    assert_eq!(small.len(), 3);
    assert_eq!(small.dropped(), 7, "every event refused under the limits is counted");
    // Draining still works and the counters follow.
    let next = raw(&m, fx.conv, seq, hlc_from_ms(fx.clock_ms + 4000), named("next"));
    let (v, applied) = pending.ingest(&mut fx.state, next, NOW);
    assert_eq!(v, Verdict::Accept);
    assert_eq!(applied.len(), 2, "the honest early event applied right after its predecessor");
    // The default buffer is unchanged: it takes everything up to the global caps.
    let mut plain = PendingBuffer::new();
    for _ in 0..300 {
        let junk = Identity::generate();
        plain.ingest(&mut fx.state, raw(&junk, fx.conv, 1, hlc_from_ms(T0), named("junk")), NOW);
    }
    assert_eq!(plain.len(), 300);
}

#[test]
fn pending_future_events_wait_for_the_clock() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let seq = fx.next_seq(&o);
    let later = raw(&o, fx.conv, seq, hlc_from_ms(NOW + MAX_FUTURE_MS + 60_000), named("from the future"));
    let after = raw(&o, fx.conv, seq + 1, hlc_from_ms(NOW + MAX_FUTURE_MS + 60_001), named("after"));
    let mut pending = PendingBuffer::new();
    assert_eq!(pending.ingest(&mut fx.state, after.clone(), NOW).0, Verdict::Gap);
    assert_eq!(pending.ingest(&mut fx.state, later.clone(), NOW).0, Verdict::Future);
    assert_eq!(pending.len(), 2);
    assert!(pending.drain_ready(&mut fx.state, NOW).is_empty());
    assert!(pending.drain_ready(&mut fx.state, NOW + 59_000).is_empty());
    let out = pending.drain_ready(&mut fx.state, NOW + 61_000);
    assert_eq!(out.iter().map(|(e, _)| e.id).collect::<Vec<_>>(), vec![later.id, after.id]);
    assert_eq!(out[0].1, Change::Members);
    assert!(pending.is_empty());
    assert_eq!(pending.bytes(), 0);
}

#[test]
fn pending_discards_stale_and_rejected() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let m = fx.member("m");
    let mut pending = PendingBuffer::new();
    // a stale copy of an applied event
    pending.insert(fx.log[1].clone());
    // a rejected event at the head position, and one after it
    let seq = fx.next_seq(&m);
    let bad = raw(&m, fx.conv, seq, hlc_from_ms(fx.clock_ms + 1000), Body::ServerUpdate { name: Some("x".into()) });
    let next = raw(&m, fx.conv, seq + 1, hlc_from_ms(fx.clock_ms + 2000), named("y"));
    pending.insert(bad.clone());
    pending.insert(next.clone());
    pending.insert(next.clone()); // duplicate key ignored
    assert_eq!(pending.len(), 3);
    assert!(pending.drain_ready(&mut fx.state, NOW).is_empty());
    assert!(!pending.contains(&o.user(), 2), "stale entry dropped");
    assert!(!pending.contains(&m.user(), seq), "rejected entry dropped");
    assert!(pending.contains(&m.user(), seq + 1), "blocked successor kept");
    pending.clear();
    assert!(pending.is_empty());
    assert_eq!(pending.bytes(), 0);
}

#[test]
fn pending_is_bounded_by_count() {
    let mut pending = PendingBuffer::new();
    let u = UserId([1; 32]);
    for seq in 1..=(MAX_PENDING_EVENTS as u64 + 5) {
        pending.insert(fake(u, seq, 1));
    }
    assert_eq!(pending.len(), MAX_PENDING_EVENTS);
    assert_eq!(pending.dropped(), 5, "evictions are counted");
    for seq in 1..=5 {
        assert!(!pending.contains(&u, seq), "oldest {seq} dropped");
    }
    assert!(pending.contains(&u, 6));
    assert!(pending.contains(&u, MAX_PENDING_EVENTS as u64 + 5));
    let expected: usize = pending.items.values().map(|e| e.approx_size()).sum();
    assert_eq!(pending.bytes(), expected);
}

#[test]
fn pending_is_bounded_by_bytes() {
    let mut pending = PendingBuffer::new();
    let u = UserId([2; 32]);
    for seq in 1..=24u64 {
        pending.insert(fake(u, seq, 1024 * 1024));
    }
    assert!(pending.bytes() <= MAX_PENDING_BYTES);
    assert!(pending.len() < 24);
    assert!(!pending.contains(&u, 1));
    assert!(pending.contains(&u, 24));
    let expected: usize = pending.items.values().map(|e| e.approx_size()).sum();
    assert_eq!(pending.bytes(), expected);
}

#[test]
fn ingest_reports_unblocked_events_in_order() {
    let mut fx = Fx::new();
    let o = fx.owner.clone();
    let j = Identity::generate();
    let p1 = raw(&j, fx.conv, 1, hlc_from_ms(fx.clock_ms + 10_000), named("j"));
    let p2 = raw(&j, fx.conv, 2, hlc_from_ms(fx.clock_ms + 11_000), text(fx.general, "hi"));
    let mut pending = PendingBuffer::new();
    assert_eq!(pending.ingest(&mut fx.state, p2.clone(), NOW).0, Verdict::Gap);
    assert_eq!(pending.ingest(&mut fx.state, p1.clone(), NOW).0, Verdict::Gap);
    let admit = fx.peek(&o, Body::MemberAdmit { user: j.user(), invite_pk: fx.invite, from_seq: 1 });
    let (v, out) = pending.ingest(&mut fx.state, admit.clone(), NOW);
    assert_eq!(v, Verdict::Accept);
    let ids: Vec<EventId> = out.iter().map(|(e, _)| e.id).collect();
    assert_eq!(ids, vec![admit.id, p1.id, p2.id]);
    assert_eq!(out[2].1, Change::Message { id: p2.id, channel: fx.general });
    // stale and rejected events are not buffered
    assert_eq!(pending.ingest(&mut fx.state, admit, NOW).0, Verdict::Stale);
    let bad = raw(&j, fx.conv, 3, hlc_from_ms(fx.clock_ms + 12_000), text(fx.voice, "x"));
    assert!(is_reject(&pending.ingest(&mut fx.state, bad, NOW).0));
    assert!(pending.is_empty());
}

#[test]
fn verdict_helpers() {
    assert!(Verdict::Accept.is_accept());
    assert!(!Verdict::Gap.is_accept());
    assert!(Verdict::Gap.should_buffer() && Verdict::Future.should_buffer());
    assert!(!Verdict::Stale.should_buffer() && !Verdict::Reject(String::new()).should_buffer());
}
