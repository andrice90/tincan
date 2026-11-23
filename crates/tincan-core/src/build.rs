//! Convenience constructors for events and handshake messages, so the network layer only
//! assembles and sends. Builders check the self-contained rules of `state` (name and text
//! limits) and return `CoreError::InvalidEvent` early; they do not check seq/hlc order or
//! membership (that is `ConvState::check`).
//!
//! Join flow (see docs/architecture.md "Invites and joining"):
//! 1. member: [`new_invite`] -> publish the InviteCreate, share `invite.encode()`.
//! 2. joiner: `Invite::decode`, then [`join_request`] -> send to the host.
//! 3. host: [`verify_join_request`] -> [`admit`] -> apply/store/fan out the admit, then the
//!    joiner's profile (`VerifiedJoin::profile`), reply [`join_accepted`].
//! 4. joiner: [`verify_join_accepted`] -> store genesis and admit, then `Hello` via [`hello`].

use crate::election::{HostClaim, SignedHostClaim, Status};
use crate::event::{Attachment, Body, ChannelKind, Event, SignedEvent, StoredEvent};
use crate::hlc::{Hlc, hlc_ms};
use crate::invite::{Invite, MAX_BOOTSTRAP, invite_secret, join_sig_message};
use crate::keys::{Identity, ServerKeys, hello_mac, random_bytes, verify, verify_hello_mac};
use crate::state::{ConvState, MAX_FUTURE_MS, dm_channel, validate_message, validate_name};
use crate::wire::{AddrHint, ClipInfo, Ctl, RejectCode};
use crate::{BlobHash, ChannelId, ConvId, CoreError, PROTOCOL_VERSION, Result, ServerId, UserId};

/// Name of the text channel every new server starts with.
pub const DEFAULT_TEXT_CHANNEL: &str = "general";
/// Name of the voice channel every new server starts with.
pub const DEFAULT_VOICE_CHANNEL: &str = "Lounge";
/// Default invite lifetime (7 days).
pub const DEFAULT_INVITE_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// Largest accepted difference between a JoinRequest's `ts_ms` and the host clock.
pub const JOIN_MAX_SKEW_MS: u64 = MAX_FUTURE_MS;

/// Sign an event by `identity`. For a Genesis body `conv` is ignored (Genesis always carries
/// `ConvId([0; 32])`).
pub fn event(identity: &Identity, conv: ConvId, seq: u64, hlc: u64, body: Body) -> Result<SignedEvent> {
    let server = if matches!(body, Body::Genesis { .. }) { ConvId::default() } else { conv };
    let ev = Event { v: PROTOCOL_VERSION, server, author: identity.user(), seq, hlc, body };
    SignedEvent::sign(identity, &ev)
}

/// The Genesis event of a new server (seq 1) and the resulting ServerId.
pub fn genesis(identity: &Identity, name: &str, hlc: u64) -> Result<(ServerId, SignedEvent)> {
    let name = valid_name(name)?;
    let signed = event(identity, ConvId::default(), 1, hlc, Body::Genesis { name, owner: identity.user(), nonce: random_bytes() })?;
    Ok((ConvId::from_event(&signed.id()), signed))
}

/// A freshly created server.
#[derive(Clone, Debug)]
pub struct NewServer {
    /// The server id (Genesis event id).
    pub server: ServerId,
    /// Random secret; store it and give it to admitted members (never put it in invites).
    pub server_secret: [u8; 32],
    /// Keys derived from the secret.
    pub keys: ServerKeys,
    /// Genesis, owner Profile, ChannelCreate "general" (Text, 0), ChannelCreate "Lounge"
    /// (Voice, 0); seqs 1..=4, increasing hlc. Apply and store in this order.
    pub events: Vec<SignedEvent>,
    /// Id of "general".
    pub text_channel: ChannelId,
    /// Id of "Lounge".
    pub voice_channel: ChannelId,
    /// The owner's next seq (5).
    pub next_seq: u64,
}

/// Create a server owned by `identity`. `owner_name` is the owner's display name for the
/// Profile event.
pub fn new_server(identity: &Identity, name: &str, owner_name: &str, hlc: &Hlc) -> Result<NewServer> {
    let owner_name = valid_name(owner_name)?;
    let (server, g) = genesis(identity, name, hlc.now())?;
    let profile = event(identity, server, 2, hlc.now(), Body::Profile { name: owner_name })?;
    let text = event(
        identity,
        server,
        3,
        hlc.now(),
        Body::ChannelCreate { name: DEFAULT_TEXT_CHANNEL.into(), kind: ChannelKind::Text, position: 0 },
    )?;
    let voice = event(
        identity,
        server,
        4,
        hlc.now(),
        Body::ChannelCreate { name: DEFAULT_VOICE_CHANNEL.into(), kind: ChannelKind::Voice, position: 0 },
    )?;
    let server_secret: [u8; 32] = random_bytes();
    Ok(NewServer {
        server,
        server_secret,
        keys: ServerKeys::derive(&server_secret, &server),
        text_channel: text.id(),
        voice_channel: voice.id(),
        events: vec![g, profile, text, voice],
        next_seq: 5,
    })
}

/// A display-name (Profile) event.
pub fn profile(identity: &Identity, conv: ConvId, seq: u64, hlc: u64, name: &str) -> Result<SignedEvent> {
    event(identity, conv, seq, hlc, Body::Profile { name: valid_name(name)? })
}

/// A channel message in a server.
pub fn message(identity: &Identity, server: ServerId, seq: u64, hlc: u64, channel: ChannelId, text: &str, attachments: Vec<Attachment>) -> Result<SignedEvent> {
    validate_message(text, &attachments).map_err(CoreError::InvalidEvent)?;
    event(identity, server, seq, hlc, Body::Message { channel, text: text.to_string(), attachments })
}

/// A message in the DM with `peer` (conv `ConvId::dm`, channel `state::dm_channel`).
pub fn dm_message(identity: &Identity, peer: &UserId, seq: u64, hlc: u64, text: &str, attachments: Vec<Attachment>) -> Result<SignedEvent> {
    let conv = ConvId::dm(&identity.user(), peer);
    message(identity, conv, seq, hlc, dm_channel(&conv), text, attachments)
}

/// Delete a message (own, or any as the owner).
pub fn delete(identity: &Identity, conv: ConvId, seq: u64, hlc: u64, target: crate::EventId) -> Result<SignedEvent> {
    event(identity, conv, seq, hlc, Body::Delete { target })
}

/// Leave a server: `MemberLeave{last_seq: seq}` (this event is the author's last).
pub fn leave(identity: &Identity, server: ServerId, seq: u64, hlc: u64) -> Result<SignedEvent> {
    event(identity, server, seq, hlc, Body::MemberLeave { last_seq: seq })
}

/// Remove a member (owner only): `MemberRemove{user, last_seq}` with `last_seq` = my head
/// for `user` in `state`, so everything of theirs I hold stays valid.
pub fn remove_member(identity: &Identity, state: &ConvState, seq: u64, hlc: u64, user: UserId) -> Result<SignedEvent> {
    event(identity, state.conv, seq, hlc, Body::MemberRemove { user, last_seq: state.head(&user) })
}

/// Create an invite valid until `expires_ms` with unlimited uses: the InviteCreate event to
/// publish and the Invite to share (`invite.encode()`). At most 2 bootstrap peers are kept.
/// An empty `name_hint` uses the server name from `state`.
#[allow(clippy::too_many_arguments)]
pub fn new_invite(
    identity: &Identity,
    state: &ConvState,
    seq: u64,
    hlc: u64,
    expires_ms: u64,
    bootstrap: Vec<(UserId, AddrHint)>,
    keys: &ServerKeys,
    name_hint: &str,
) -> Result<(SignedEvent, Invite)> {
    new_invite_with_limit(identity, state, seq, hlc, InviteOptions { expires_ms, max_uses: 0, bootstrap, name_hint: name_hint.to_string() }, keys)
}

/// Options for [`new_invite_with_limit`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InviteOptions {
    /// Unix ms after which the invite stops working.
    pub expires_ms: u64,
    /// Admission limit, 0 = unlimited.
    pub max_uses: u32,
    /// Up to 2 peers for the joiner to dial first (inviter, current host).
    pub bootstrap: Vec<(UserId, AddrHint)>,
    /// Server name shown to the joiner; empty = `state.name`.
    pub name_hint: String,
}

/// Same as [`new_invite`] with a use limit.
pub fn new_invite_with_limit(identity: &Identity, state: &ConvState, seq: u64, hlc: u64, opts: InviteOptions, keys: &ServerKeys) -> Result<(SignedEvent, Invite)> {
    if state.is_dm {
        return Err(CoreError::InvalidInvite("DMs have no invites".into()));
    }
    let server = state.conv;
    let seed: [u8; 16] = random_bytes();
    let invite_pk = *invite_secret(&seed, &server).public().as_bytes();
    let ev = event(identity, server, seq, hlc, Body::InviteCreate { invite_pk, expires_ms: opts.expires_ms, max_uses: opts.max_uses })?;
    let mut bootstrap = opts.bootstrap;
    bootstrap.truncate(MAX_BOOTSTRAP);
    let name_hint = if opts.name_hint.trim().is_empty() { state.name.clone() } else { opts.name_hint };
    let invite = Invite {
        v: PROTOCOL_VERSION,
        server,
        name_hint,
        invite_seed: seed,
        rdv_pk: *keys.rdv_pk().as_bytes(),
        rdv_read_key: keys.rdv_read_key,
        bootstrap,
        expires_ms: opts.expires_ms,
    };
    Ok((ev, invite))
}

/// Revoke an invite.
pub fn revoke_invite(identity: &Identity, server: ServerId, seq: u64, hlc: u64, invite_pk: [u8; 32]) -> Result<SignedEvent> {
    event(identity, server, seq, hlc, Body::InviteRevoke { invite_pk })
}

/// Joiner side: the `Ctl::JoinRequest` for `host`, carrying the joiner's Profile event at
/// `seq` (1 on a first join; the next seq after their leave on a re-join).
pub fn join_request(joiner: &Identity, invite: &Invite, host: &UserId, display_name: &str, seq: u64, hlc: u64, ts_ms: u64) -> Result<Ctl> {
    let profile = profile(joiner, invite.server, seq, hlc, display_name)?;
    Ok(Ctl::JoinRequest {
        server: invite.server,
        invite_pk: invite.invite_pk(),
        profile,
        ts_ms,
        invite_sig: invite.sign_join(&joiner.user(), host, ts_ms),
    })
}

/// Why a host turned a joiner away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinRejection {
    /// For `Ctl::Reject`.
    pub code: RejectCode,
    /// Short reason (user-facing for invite problems, log text otherwise).
    pub reason: String,
}

impl JoinRejection {
    fn new(code: RejectCode, reason: impl Into<String>) -> Self { Self { code, reason: reason.into() } }
    /// The `Ctl::Reject` to send.
    pub fn to_ctl(&self) -> Ctl { Ctl::Reject { code: self.code, reason: self.reason.clone() } }
}

impl std::fmt::Display for JoinRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{:?}: {}", self.code, self.reason) }
}

impl std::error::Error for JoinRejection {}

/// A JoinRequest the host has checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedJoin {
    /// The joiner (author of the profile event; the caller should also check it equals the
    /// authenticated QUIC peer id).
    pub joiner: UserId,
    /// The invite used.
    pub invite_pk: [u8; 32],
    /// The joiner's Profile event, to apply right after the admit.
    pub profile: StoredEvent,
    /// The joiner is already a member (a retried request): do not admit again, reply with
    /// the existing admit.
    pub already_member: bool,
}

/// Host side: check a `Ctl::JoinRequest` against the server state. `host` is the local user.
/// Checks the server id, the profile event (signature, server, seq, name, clock), the
/// timestamp (within `JOIN_MAX_SKEW_MS` of `now_ms`), the invite signature, and that the
/// invite can still admit (`ConvState::invite_status`), unless the joiner already is a member.
pub fn verify_join_request(ctl: &Ctl, state: &ConvState, host: &UserId, now_ms: u64) -> Result<VerifiedJoin, JoinRejection> {
    let Ctl::JoinRequest { server, invite_pk, profile, ts_ms, invite_sig } = ctl else {
        return Err(JoinRejection::new(RejectCode::Internal, "not a join request"));
    };
    if state.is_dm || *server != state.conv {
        return Err(JoinRejection::new(RejectCode::BadInvite, "invite is for another server"));
    }
    if state.owner.is_none() {
        return Err(JoinRejection::new(RejectCode::Internal, "server state not loaded"));
    }
    let profile = StoredEvent::from_signed(profile.clone()).map_err(|_| JoinRejection::new(RejectCode::BadInvite, "bad profile event"))?;
    let joiner = profile.event.author;
    let Body::Profile { name } = &profile.event.body else {
        return Err(JoinRejection::new(RejectCode::BadInvite, "join must carry a profile event"));
    };
    if profile.event.server != *server || profile.event.v != PROTOCOL_VERSION {
        return Err(JoinRejection::new(RejectCode::BadInvite, "profile event for another server"));
    }
    validate_name(name).map_err(|r| JoinRejection::new(RejectCode::BadInvite, r))?;
    if hlc_ms(profile.event.hlc) > now_ms.saturating_add(MAX_FUTURE_MS) {
        return Err(JoinRejection::new(RejectCode::BadInvite, "your clock is too far ahead"));
    }
    if now_ms.abs_diff(*ts_ms) > JOIN_MAX_SKEW_MS {
        return Err(JoinRejection::new(RejectCode::BadInvite, "your clock is off by more than 5 minutes"));
    }
    verify(&UserId::from_bytes(*invite_pk), &join_sig_message(server, &joiner, host, *ts_ms), invite_sig)
        .map_err(|_| JoinRejection::new(RejectCode::BadInvite, "bad invite signature"))?;
    let already_member = state.is_member(&joiner);
    if !already_member {
        if let Err(p) = state.invite_status(invite_pk, now_ms) {
            return Err(JoinRejection::new(p.reject_code(), p.message()));
        }
        if profile.event.seq != state.next_seq(&joiner) {
            return Err(JoinRejection::new(RejectCode::BadInvite, "profile event has the wrong seq"));
        }
        // A re-admit clears a removal only from past its cut. A host that is behind on the
        // removed member's own events would admit them without clearing it.
        if state.active_removal(&joiner).is_some_and(|r| profile.event.seq <= r.last_seq) {
            return Err(JoinRejection::new(RejectCode::Busy, "this host is still catching up; try again in a minute"));
        }
    }
    Ok(VerifiedJoin { joiner, invite_pk: *invite_pk, profile, already_member })
}

/// Host side: the `MemberAdmit` for a verified join (`from_seq` = the profile's seq).
/// `CoreError::InvalidInvite` if the invite is unknown or already expired at `hlc` (the admit
/// would be rejected by validation, which judges expiry at the admit's hlc).
pub fn admit(host: &Identity, state: &ConvState, seq: u64, hlc: u64, join: &VerifiedJoin) -> Result<SignedEvent> {
    match state.invites.get(&join.invite_pk) {
        Some(i) if hlc_ms(hlc) > i.expires_ms => return Err(CoreError::InvalidInvite("invite expired".into())),
        Some(_) => {}
        None => return Err(CoreError::InvalidInvite("unknown invite".into())),
    }
    event(host, state.conv, seq, hlc, Body::MemberAdmit { user: join.joiner, invite_pk: join.invite_pk, from_seq: join.profile.event.seq })
}

/// Host side: the reply to an accepted join.
pub fn join_accepted(server_secret: &[u8; 32], genesis: &SignedEvent, admit: &SignedEvent) -> Ctl {
    Ctl::JoinAccepted { server_secret: *server_secret, genesis: genesis.clone(), admit: admit.clone() }
}

/// What a joiner learns from `Ctl::JoinAccepted`.
#[derive(Clone, Debug)]
pub struct AcceptedJoin {
    /// The server.
    pub server: ServerId,
    /// The server secret (store it).
    pub server_secret: [u8; 32],
    /// Keys derived from the secret.
    pub keys: ServerKeys,
    /// The verified Genesis event.
    pub genesis: StoredEvent,
    /// The verified MemberAdmit naming the joiner.
    pub admit: StoredEvent,
}

/// Joiner side: check `Ctl::JoinAccepted` against the invite: the genesis is the invited
/// server, the admit names `joiner` and the invite, and the secret derives the invite's
/// rendezvous keys (so the host cannot hand out a wrong secret).
pub fn verify_join_accepted(ctl: &Ctl, invite: &Invite, joiner: &UserId) -> Result<AcceptedJoin> {
    let bad = |why: &str| CoreError::InvalidInvite(why.to_string());
    let Ctl::JoinAccepted { server_secret, genesis, admit } = ctl else {
        return Err(bad("not a join acceptance"));
    };
    let genesis = StoredEvent::from_signed(genesis.clone())?;
    if !matches!(genesis.event.body, Body::Genesis { .. }) || genesis.conv() != invite.server {
        return Err(bad("host sent the wrong server"));
    }
    let admit = StoredEvent::from_signed(admit.clone())?;
    match &admit.event.body {
        Body::MemberAdmit { user, invite_pk, .. } if user == joiner && *invite_pk == invite.invite_pk() && admit.event.server == invite.server => {}
        _ => return Err(bad("host sent a wrong admission")),
    }
    let keys = ServerKeys::derive(server_secret, &invite.server);
    if *keys.rdv_pk().as_bytes() != invite.rdv_pk || keys.rdv_read_key != invite.rdv_read_key {
        return Err(bad("host sent a wrong server secret"));
    }
    Ok(AcceptedJoin { server: invite.server, server_secret: *server_secret, keys, genesis, admit })
}

/// The non-MAC fields of a `Ctl::Hello`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HelloInfo {
    /// Highest host epoch seen.
    pub epoch_seen: u64,
    /// Current status.
    pub status: Status,
    /// Complete blobs held.
    pub have: Vec<BlobHash>,
    /// Soundboard library.
    pub clips: Vec<ClipInfo>,
}

/// Member side: a `Ctl::Hello` to `host` with a fresh nonce and the membership MAC.
pub fn hello(client: &UserId, server: &ServerId, hello_key: &[u8; 32], host: &UserId, info: HelloInfo) -> Ctl {
    let nonce: [u8; 16] = random_bytes();
    Ctl::Hello {
        server: *server,
        nonce,
        mac: hello_mac(hello_key, server, client, host, &nonce),
        epoch_seen: info.epoch_seen,
        status: info.status,
        have: info.have,
        clips: info.clips,
    }
}

/// Host side: check a `Ctl::Hello` MAC. `client` is the authenticated QUIC peer id, `host` the
/// local id, `hello_key` the key of the server named in the Hello. False for any other Ctl.
pub fn verify_hello(ctl: &Ctl, hello_key: &[u8; 32], client: &UserId, host: &UserId) -> bool {
    match ctl {
        Ctl::Hello { server, nonce, mac, .. } => verify_hello_mac(hello_key, server, client, host, nonce, mac),
        _ => false,
    }
}

/// A signed claim that `identity` hosts `server` for `epoch`.
pub fn host_claim(identity: &Identity, server: &ServerId, epoch: u64, prev: Option<UserId>, issued_ms: u64) -> Result<SignedHostClaim> {
    SignedHostClaim::sign(identity, HostClaim { server: *server, epoch, host: identity.user(), prev, issued_ms })
}

fn valid_name(name: &str) -> Result<String> {
    let name = name.trim();
    validate_name(name).map_err(CoreError::InvalidEvent)?;
    Ok(name.to_string())
}

#[cfg(test)]
mod tests;
