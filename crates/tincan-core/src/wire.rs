//! Control messages and stream framing for `tincan/server/1` and `tincan/dm/1`.
//!
//! Frame = `u32 little-endian length || postcard(Ctl)`, length <= MAX_FRAME.
//! Append new `Ctl` variants only at the end.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::election::{MAX_ATTESTS, SignedAttestation, SignedHostClaim, Status};
use crate::event::SignedEvent;
use crate::keys::Sig64;
use crate::sync::Heads;
use crate::{BlobHash, ChannelId, ConvId, CoreError, EventId, Result, ServerId, UserId};

/// Largest frame body in bytes.
pub const MAX_FRAME: usize = 1024 * 1024;

/// Where to reach a peer: iroh relay URL plus direct socket addresses.
/// Converted to/from `iroh::EndpointAddr` in tincan-net.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddrHint {
    /// Home relay URL.
    pub relay: Option<String>,
    /// Direct UDP addresses.
    pub direct: Vec<SocketAddr>,
}

/// A soundboard clip as announced to others.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClipInfo {
    /// Blob hash of the clip file.
    pub hash: BlobHash,
    /// Display name.
    pub name: String,
    /// File size in bytes.
    pub size: u64,
    /// Decoded length.
    pub duration_ms: u32,
}

/// One online member as the host sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterEntry {
    /// The member.
    pub user: UserId,
    /// Host-assigned, kept across migrations. Media track ids derive from it.
    pub short_id: u16,
    /// How the host reaches them.
    pub addr: AddrHint,
    /// Online since (unix ms, host clock).
    pub since_ms: u64,
    /// Voice room they are in.
    pub voice: Option<ChannelId>,
    /// Microphone muted.
    pub muted: bool,
    /// Deafened.
    pub deafened: bool,
    /// Track id of their active screen share, if any.
    pub sharing: Option<u16>,
    /// The host saw a selected direct (non-relay) path to this member.
    pub direct: bool,
    /// Round-trip time host <-> member.
    pub rtt_ms: u16,
}

/// Who is online, as broadcast by the host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    /// Host epoch.
    pub epoch: u64,
    /// +1 per roster change within an epoch.
    pub rev: u32,
    /// Current host.
    pub host: UserId,
    /// Online members (the host included).
    pub online: Vec<RosterEntry>,
    /// Up to 4 successors in order.
    pub succession: Vec<UserId>,
}

/// Why a peer was turned away.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectCode {
    /// Hello MAC or membership check failed.
    NotMember,
    /// Unknown, revoked or used-up invite, or a bad invite signature.
    BadInvite,
    /// The invite has expired.
    InviteExpired,
    /// The dialed peer is not the host (and does not know who is).
    NotHost,
    /// Incompatible protocol version.
    Version,
    /// Something broke on the host.
    Internal,
    /// The host is busy (too many joins right now); try again in a minute. Appended last
    /// (postcard encodes the index).
    Busy,
}

/// Control messages. Append new variants only at the end (postcard encodes the index).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ctl {
    // ---- session setup (server) ----
    /// First frame from a member to whoever it dialed.
    Hello {
        /// The server.
        server: ServerId,
        /// Random per Hello.
        nonce: [u8; 16],
        /// keys::hello_mac(hello_key, server, client, host, nonce)
        mac: [u8; 32],
        /// Highest host epoch the member has seen.
        epoch_seen: u64,
        /// The member's status.
        status: Status,
        /// Blob hashes this member holds completely (attachments and clips).
        have: Vec<BlobHash>,
        /// Soundboard clips in the member's library.
        clips: Vec<ClipInfo>,
    },
    /// First frame from a joiner who holds only an invite.
    JoinRequest {
        /// The server (from the invite).
        server: ServerId,
        /// The invite's public key.
        invite_pk: [u8; 32],
        /// The joiner's Profile event (seq 1 on a first join).
        profile: SignedEvent,
        /// Joiner's clock, unix ms.
        ts_ms: u64,
        /// invite::join_sig_message(...) signed by the invite secret key.
        invite_sig: Sig64,
    },
    /// Host -> joiner: you are in.
    JoinAccepted {
        /// The server secret (all server keys derive from it).
        server_secret: [u8; 32],
        /// The Genesis event (its id is the server id).
        genesis: SignedEvent,
        /// The MemberAdmit event naming the joiner.
        admit: SignedEvent,
    },
    /// Reply to Hello from the host.
    Welcome {
        /// The host's claim for the current epoch.
        claim: SignedHostClaim,
        /// Current roster.
        roster: Roster,
        /// The member's short id (media track ids).
        your_short_id: u16,
        /// The seq the member should use next (host's view of its head + 1).
        your_next_seq: u64,
    },
    /// Reply to Hello/JoinRequest from a non-host: go there instead.
    Redirect {
        /// The real host's claim.
        claim: SignedHostClaim,
        /// The real host.
        host: UserId,
        /// How to reach it.
        addr: AddrHint,
    },
    /// The peer was turned away (the connection closes with `CLOSE_REJECTED`).
    Reject {
        /// Machine-readable reason.
        code: RejectCode,
        /// Log text.
        reason: String,
    },

    // ---- history ----
    /// Per-author heads, sent by both sides on connect.
    SyncHave {
        /// Highest contiguous seq per author.
        heads: Heads,
    },
    /// A page of events (live fan-out or sync).
    Events {
        /// Signed events, per author ascending seq.
        events: Vec<SignedEvent>,
        /// More sync pages follow.
        more: bool,
    },
    /// Member -> host: validate, store and fan out this event.
    Publish {
        /// The member's new event.
        event: SignedEvent,
    },
    /// Host -> member: result of a Publish.
    PublishAck {
        /// Id of the published event.
        id: EventId,
        /// Accepted.
        ok: bool,
        /// Log text when not ok.
        reason: String,
    },
    /// Periodic digest of heads (`Heads::digest`); a mismatch triggers SyncHave.
    VvDigest {
        /// The digest.
        digest: [u8; 32],
    },

    // ---- presence and election ----
    /// Member status changed.
    Status(Status),
    /// Host -> members: current roster.
    Roster(Roster),
    /// Liveness, every `election::TICK_MS` both ways.
    Tick {
        /// Sender's view of the host epoch.
        epoch: u64,
        /// Sender clock, unix ms.
        t_ms: u64,
    },
    /// Host -> first successor: take over.
    Handoff {
        /// The sending host's current claim (proves it is the host). A claim is signed by the
        /// host it names, so the successor then issues its own claim for `epoch + 1` with
        /// `prev = Some(sender)` and announces it in `HandoffAck` / `HostMoved`.
        claim: SignedHostClaim,
        /// Roster to inherit (short ids survive).
        roster: Roster,
    },
    /// Successor -> host: handoff accepted.
    HandoffAck {
        /// The new epoch.
        epoch: u64,
    },
    /// Host -> members: the host moved.
    HostMoved {
        /// Claim of the new host.
        claim: SignedHostClaim,
        /// How to reach it.
        addr: AddrHint,
    },
    /// Host <-> host during partition merge.
    HostCompare {
        /// Sender's claim.
        claim: SignedHostClaim,
        /// Members connected to the sender.
        connected: u32,
        /// Sender's election score.
        score: u32,
    },

    // ---- files ----
    /// Held blobs changed.
    Have {
        /// Newly complete blobs.
        add: Vec<BlobHash>,
        /// Dropped blobs.
        remove: Vec<BlobHash>,
    },
    /// Member -> host: who holds this blob?
    WhoHas {
        /// The blob.
        hash: BlobHash,
    },
    /// Host -> member: online holders.
    Holders {
        /// The blob.
        hash: BlobHash,
        /// Holders and their addresses.
        holders: Vec<(UserId, AddrHint)>,
    },

    // ---- voice and screen ----
    /// Member joins a voice room.
    VoiceJoin {
        /// The voice channel.
        channel: ChannelId,
    },
    /// Member leaves its voice room.
    VoiceLeave,
    /// Start receiving a video track.
    Subscribe {
        /// Track id.
        track: u16,
    },
    /// Stop receiving a video track.
    Unsubscribe {
        /// Track id.
        track: u16,
    },
    /// Ask the sharer for an IDR.
    KeyframeRequest {
        /// Track id.
        track: u16,
    },
    /// Host -> sharer: encoder bitrate cap.
    TargetBitrate {
        /// Track id.
        track: u16,
        /// Bits per second.
        bps: u32,
    },
    /// Receiver statistics for a track.
    ReceiverReport {
        /// Track id.
        track: u16,
        /// Packets received.
        rx: u32,
        /// Packets lost.
        lost: u32,
        /// Packets too late to play.
        late: u32,
        /// Interarrival jitter.
        jitter_ms: u16,
    },
    /// Member -> host -> everyone in the same voice room.
    SoundPlayed {
        /// Who played it.
        by: UserId,
        /// The clip.
        clip: ClipInfo,
    },
    /// Someone is typing in a channel.
    Typing {
        /// The channel.
        channel: ChannelId,
        /// The typist.
        by: UserId,
    },

    // ---- direct messages and 1:1 calls (tincan/dm/1) ----
    /// First frame on a DM connection (both sides).
    DmHello {
        /// The DM conversation (`ConvId::dm`).
        conv: ConvId,
        /// Sender's heads for that conversation.
        heads: Heads,
        /// Blobs the sender holds.
        have: Vec<BlobHash>,
    },
    /// Ring the other side.
    CallInvite {
        /// Random call id.
        call_id: u64,
    },
    /// Answer.
    CallAccept {
        /// Call id.
        call_id: u64,
    },
    /// Decline.
    CallDecline {
        /// Call id.
        call_id: u64,
    },
    /// Hang up.
    CallEnd {
        /// Call id.
        call_id: u64,
    },
    /// In-call state of the sender.
    CallState {
        /// Call id.
        call_id: u64,
        /// Mic muted.
        muted: bool,
        /// Deafened.
        deafened: bool,
        /// Screen share track id, if sharing.
        sharing: Option<u16>,
    },

    // ---- appended (hardening) ----
    /// Member -> host: "I am connected to you at your current epoch" (signed by the member).
    /// Sent after Welcome, after every HostMoved and every `ATTEST_EVERY_MS`.
    Attest(SignedAttestation),
    /// Host <-> host during partition merge: like `HostCompare`, but the member count is
    /// proven by member attestations (at most `MAX_ATTESTS`) instead of self-reported.
    HostCompare2 {
        /// Sender's claim.
        claim: SignedHostClaim,
        /// Attestations of the sender's connected members for that claim's epoch.
        attests: Vec<SignedAttestation>,
    },
    /// Heads too large for one frame, split into parts. The receiver collects parts until
    /// `more == false` and then handles the union like one `SyncHave`.
    SyncHavePart {
        /// Part of the heads.
        heads: Heads,
        /// More parts follow.
        more: bool,
    },
    /// DM connections: the sender's display name. Sent by both sides once the DM connection
    /// is up, and again when the name changes. Receivers keep it only if it passes
    /// `state::validate_name` after trimming. Peers that predate this variant skip the frame
    /// (`FrameDecoder` consumes undecodable frames).
    DmProfile {
        /// The sender's display name.
        name: String,
    },
}

/// Longest soundboard clip name, in chars (same as other names).
pub const MAX_CLIP_NAME_CHARS: usize = 100;
/// Largest soundboard clip file (architecture.md: 5 MB).
pub const MAX_CLIP_BYTES: u64 = 5 * 1024 * 1024;
/// Longest soundboard clip (architecture.md: 20 s).
pub const MAX_CLIP_MS: u32 = 20_000;
/// Decoder padding slack on top of `MAX_CLIP_MS` (same as `tincan_audio::CLIP_LENGTH_TOLERANCE_MS`:
/// an MP3 trimmed to exactly 20 s decodes a little longer, and the app stores what it decoded).
pub const CLIP_DURATION_TOLERANCE_MS: u32 = 100;
/// Most clips a member may announce.
pub const MAX_CLIPS: usize = 256;
/// Most held-blob hashes a host keeps per member (and that one Hello/DmHello may carry).
pub const MAX_HAVE: usize = 20_000;
/// Most hashes in one `Have` delta (each of add and remove).
pub const MAX_HAVE_DELTA: usize = 1024;
/// Most entries in a roster a member accepts.
pub const MAX_ROSTER: usize = 256;
/// Most holders in one `Holders` reply a member accepts.
pub const MAX_HOLDERS: usize = 32;
/// Longest reason text in `Reject` / `PublishAck`.
pub const MAX_REASON_CHARS: usize = 256;
/// Longest app version string in `Status`.
pub const MAX_APP_VERSION_CHARS: usize = 32;
/// Most direct addresses kept from one `AddrHint`.
pub const MAX_HINT_DIRECT: usize = 8;
/// Longest relay URL kept from one `AddrHint`, in bytes.
pub const MAX_HINT_RELAY_BYTES: usize = 256;
/// Heads entries per `SyncHave` / `SyncHavePart` frame (about 34 B each, well below
/// `MAX_FRAME`).
pub const HEADS_PER_FRAME: usize = 8192;

impl ClipInfo {
    /// Within the soundboard limits: a short name without control characters, at most
    /// `MAX_CLIP_BYTES` and `MAX_CLIP_MS` (+ `CLIP_DURATION_TOLERANCE_MS`).
    pub fn is_valid(&self) -> bool {
        !self.name.is_empty()
            && self.name.chars().count() <= MAX_CLIP_NAME_CHARS
            && !self.name.chars().any(char::is_control)
            && self.size <= MAX_CLIP_BYTES
            && self.duration_ms <= MAX_CLIP_MS + CLIP_DURATION_TOLERANCE_MS
    }
}

impl AddrHint {
    /// The hint cut to what is ever used: at most `MAX_HINT_DIRECT` direct addresses and a
    /// relay URL of at most `MAX_HINT_RELAY_BYTES` (dropped if longer).
    pub fn bounded(&self) -> AddrHint {
        AddrHint {
            relay: self.relay.clone().filter(|r| r.len() <= MAX_HINT_RELAY_BYTES),
            direct: self.direct.iter().take(MAX_HINT_DIRECT).copied().collect(),
        }
    }

    fn within_limits(&self) -> bool { self.relay.as_ref().is_none_or(|r| r.len() <= MAX_HINT_RELAY_BYTES) && self.direct.len() <= 4 * MAX_HINT_DIRECT }
}

/// `text` safe to show a user: control characters removed, at most `max` chars.
pub fn sanitize_text(text: &str, max: usize) -> String { text.chars().filter(|c| !c.is_control()).take(max).collect() }

impl Ctl {
    /// Size limits a well-formed message never exceeds (names, lists, reasons). Receivers
    /// drop messages that fail this before acting on them. Lists an honest peer may grow
    /// (held blobs and clips in `Hello`) are not checked here: receivers truncate them.
    pub fn within_limits(&self) -> bool {
        let reason_ok = |r: &str| r.len() <= 4 * MAX_REASON_CHARS;
        match self {
            Ctl::Hello { status, .. } | Ctl::Status(status) => status.app_version.len() <= 4 * MAX_APP_VERSION_CHARS,
            // A PublishAck is kept whatever its reason (the receiver cleans the text): dropping
            // it would leave the event unacknowledged.
            Ctl::Reject { reason, .. } => reason_ok(reason),
            Ctl::Roster(r) => r.online.len() <= MAX_ROSTER && r.succession.len() <= 16 && r.online.iter().all(|e| e.addr.within_limits()),
            Ctl::Redirect { addr, .. } | Ctl::HostMoved { addr, .. } => addr.within_limits(),
            Ctl::Have { add, remove } => add.len() <= MAX_HAVE_DELTA && remove.len() <= MAX_HAVE_DELTA,
            Ctl::Holders { holders, .. } => holders.len() <= 4 * MAX_HOLDERS && holders.iter().all(|(_, a)| a.within_limits()),
            Ctl::SoundPlayed { clip, .. } => clip.is_valid(),
            Ctl::HostCompare2 { attests, .. } => attests.len() <= MAX_ATTESTS,
            Ctl::SyncHave { heads } | Ctl::SyncHavePart { heads, .. } => heads.0.len() <= 4 * HEADS_PER_FRAME,
            Ctl::DmProfile { name } => name.len() <= 4 * crate::state::MAX_NAME_CHARS,
            _ => true,
        }
    }
}

/// A `Have` delta as frames of at most `MAX_HAVE_DELTA` hashes each way.
pub fn have_frames(add: &[BlobHash], remove: &[BlobHash]) -> Vec<Ctl> {
    let mut out = Vec::new();
    for c in remove.chunks(MAX_HAVE_DELTA) {
        out.push(Ctl::Have { add: Vec::new(), remove: c.to_vec() });
    }
    for c in add.chunks(MAX_HAVE_DELTA) {
        out.push(Ctl::Have { add: c.to_vec(), remove: Vec::new() });
    }
    out
}

/// `SyncHave` for `heads`, split into `SyncHavePart` frames when it would not fit in one.
pub fn sync_have_frames(heads: Heads) -> Vec<Ctl> {
    if heads.0.len() <= HEADS_PER_FRAME {
        return vec![Ctl::SyncHave { heads }];
    }
    let chunks: Vec<Vec<(UserId, u64)>> = heads.0.chunks(HEADS_PER_FRAME).map(|c| c.to_vec()).collect();
    let last = chunks.len() - 1;
    chunks.into_iter().enumerate().map(|(i, c)| Ctl::SyncHavePart { heads: Heads(c), more: i < last }).collect()
}

/// Collects `SyncHavePart` frames (bounded).
#[derive(Debug, Default)]
pub struct HeadsAssembler {
    parts: Vec<(UserId, u64)>,
}

impl HeadsAssembler {
    /// Most entries collected; beyond that the assembly is dropped.
    pub const MAX_ENTRIES: usize = 65_536;

    /// Add a part. Returns the complete heads once `more == false`.
    pub fn push(&mut self, heads: Heads, more: bool) -> Option<Heads> {
        if self.parts.len() + heads.0.len() > Self::MAX_ENTRIES {
            self.parts.clear();
            return None;
        }
        self.parts.extend(heads.0);
        if more { None } else { Some(Heads(std::mem::take(&mut self.parts))) }
    }
}

/// Encode one frame (length prefix + postcard).
pub fn encode_frame(msg: &Ctl) -> Result<Vec<u8>> {
    let body = postcard::to_stdvec(msg)?;
    if body.len() > MAX_FRAME {
        return Err(CoreError::FrameTooLarge(body.len()));
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode a frame body (without the length prefix).
pub fn decode_body(body: &[u8]) -> Result<Ctl> { Ok(postcard::from_bytes(body)?) }

/// Incremental frame decoder for byte streams.
///
/// `next` returns `Err(FrameTooLarge)` on an oversized length prefix; the stream is then
/// unusable and should be closed. A frame whose body fails to decode (for example an unknown
/// variant from a newer peer) is consumed before the error is returned, so the caller may
/// log it and keep reading.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    /// Start of unread data in `buf`.
    pos: usize,
}

impl FrameDecoder {
    /// Empty decoder.
    pub fn new() -> Self { Self::default() }
    /// Append bytes read from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.pos > 0 && self.pos >= self.buf.len() / 2 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        self.buf.extend_from_slice(bytes);
    }
    /// Bytes buffered but not yet returned as messages.
    pub fn buffered(&self) -> usize { self.buf.len() - self.pos }
    /// Next complete message, if any.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Ctl>> {
        let avail = &self.buf[self.pos..];
        if avail.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([avail[0], avail[1], avail[2], avail[3]]) as usize;
        if len > MAX_FRAME {
            return Err(CoreError::FrameTooLarge(len));
        }
        if avail.len() < 4 + len {
            return Ok(None);
        }
        let decoded = decode_body(&avail[4..4 + len]);
        self.pos += 4 + len;
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
        decoded.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::election::HostClaim;
    use crate::keys::Identity;

    fn samples() -> Vec<Ctl> {
        let id = Identity::generate();
        let claim = crate::election::SignedHostClaim::sign(
            &id,
            HostClaim { server: ServerId::from_bytes([1; 32]), epoch: 3, host: id.user(), prev: None, issued_ms: 99 },
        )
        .unwrap();
        vec![
            Ctl::Hello {
                server: ServerId::from_bytes([1; 32]),
                nonce: [2; 16],
                mac: [3; 32],
                epoch_seen: 4,
                status: Status { app_version: "0.1.0".into(), uptime_s: 5, ac_power: true, ..Default::default() },
                have: vec![BlobHash([4; 32])],
                clips: vec![ClipInfo { hash: BlobHash([5; 32]), name: "boing".into(), size: 1000, duration_ms: 900 }],
            },
            Ctl::Welcome { claim: claim.clone(), roster: Roster::default(), your_short_id: 7, your_next_seq: 2 },
            Ctl::HostCompare { claim, connected: 3, score: 600 },
            Ctl::Tick { epoch: 1, t_ms: 2 },
            Ctl::VoiceLeave,
            Ctl::Events { events: vec![], more: true },
            Ctl::Reject { code: RejectCode::InviteExpired, reason: "late".into() },
            Ctl::Roster(Roster {
                epoch: 2,
                rev: 9,
                host: UserId([1; 32]),
                online: vec![RosterEntry {
                    user: UserId([1; 32]),
                    short_id: 1,
                    addr: AddrHint { relay: Some("https://relay.example/".into()), direct: vec!["127.0.0.1:4433".parse().unwrap()] },
                    since_ms: 5,
                    voice: Some(EventId([8; 32])),
                    muted: true,
                    deafened: false,
                    sharing: Some(5),
                    direct: true,
                    rtt_ms: 30,
                }],
                succession: vec![UserId([2; 32])],
            }),
            Ctl::CallState { call_id: 77, muted: false, deafened: true, sharing: None },
        ]
    }

    #[test]
    fn frame_round_trip() {
        for msg in samples() {
            let f = encode_frame(&msg).unwrap();
            let len = u32::from_le_bytes([f[0], f[1], f[2], f[3]]) as usize;
            assert_eq!(len + 4, f.len());
            assert_eq!(decode_body(&f[4..]).unwrap(), msg);
        }
    }

    #[test]
    fn incremental_decode_with_split_pushes() {
        let msgs = samples();
        let mut stream = Vec::new();
        for m in &msgs {
            stream.extend_from_slice(&encode_frame(m).unwrap());
        }
        for chunk in [1usize, 2, 3, 5, 13, 64, stream.len()] {
            let mut d = FrameDecoder::new();
            let mut got = Vec::new();
            for piece in stream.chunks(chunk) {
                d.push(piece);
                while let Some(m) = d.next().unwrap() {
                    got.push(m);
                }
            }
            assert_eq!(got, msgs, "chunk size {chunk}");
            assert_eq!(d.buffered(), 0);
        }
    }

    #[test]
    fn partial_frames_wait() {
        let f = encode_frame(&Ctl::Tick { epoch: 1, t_ms: 2 }).unwrap();
        let mut d = FrameDecoder::new();
        d.push(&f[..2]);
        assert!(d.next().unwrap().is_none());
        d.push(&f[2..f.len() - 1]);
        assert!(d.next().unwrap().is_none());
        assert_eq!(d.buffered(), f.len() - 1);
        d.push(&f[f.len() - 1..]);
        assert_eq!(d.next().unwrap(), Some(Ctl::Tick { epoch: 1, t_ms: 2 }));
        assert!(d.next().unwrap().is_none());
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let mut d = FrameDecoder::new();
        d.push(&((MAX_FRAME as u32) + 1).to_le_bytes());
        assert!(matches!(d.next(), Err(CoreError::FrameTooLarge(_))));
        let huge = Ctl::Events {
            events: vec![SignedEvent { bytes: vec![0; MAX_FRAME], sig: Sig64::ZERO }],
            more: false,
        };
        assert!(matches!(encode_frame(&huge), Err(CoreError::FrameTooLarge(_))));
    }

    #[test]
    fn undecodable_frame_is_skipped() {
        let mut d = FrameDecoder::new();
        let garbage = [0xFFu8, 0xFF, 0xFF];
        d.push(&(garbage.len() as u32).to_le_bytes());
        d.push(&garbage);
        d.push(&encode_frame(&Ctl::VoiceLeave).unwrap());
        assert!(d.next().is_err());
        assert_eq!(d.next().unwrap(), Some(Ctl::VoiceLeave));
    }

    #[test]
    fn variant_indices_are_stable() {
        let idx = |c: &Ctl| postcard::to_stdvec(c).unwrap()[0];
        assert_eq!(idx(&Ctl::SyncHave { heads: Heads::default() }), 6);
        assert_eq!(idx(&Ctl::Tick { epoch: 0, t_ms: 0 }), 13);
        assert_eq!(idx(&Ctl::CallState { call_id: 0, muted: false, deafened: false, sharing: None }), 35);
        assert_eq!(idx(&Ctl::SyncHavePart { heads: Heads::default(), more: false }), 38);
        assert_eq!(idx(&Ctl::DmProfile { name: String::new() }), 39);
    }

    #[test]
    fn dm_profile_round_trips_and_is_bounded() {
        let msg = Ctl::DmProfile { name: "Bea".into() };
        let f = encode_frame(&msg).unwrap();
        assert_eq!(decode_body(&f[4..]).unwrap(), msg);
        assert!(msg.within_limits());
        assert!(!Ctl::DmProfile { name: "n".repeat(4 * crate::state::MAX_NAME_CHARS + 1) }.within_limits());
        // A peer that predates the variant sees an unknown index: it skips the frame and keeps
        // reading.
        let mut unknown = f.clone();
        unknown[4] = 0x7f;
        let mut d = FrameDecoder::new();
        d.push(&unknown);
        d.push(&encode_frame(&Ctl::VoiceLeave).unwrap());
        assert!(d.next().is_err());
        assert_eq!(d.next().unwrap(), Some(Ctl::VoiceLeave));
    }

    /// Why the DM name is a new variant and not a field appended to `DmHello`: postcard is
    /// not self-describing, so a reader with the extra field cannot decode an old frame.
    #[test]
    fn appending_a_field_would_break_old_frames() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        enum Old {
            A { x: u32 },
        }
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        enum New {
            A {
                x: u32,
                #[serde(default)]
                name: Option<String>,
            },
        }
        let old = postcard::to_stdvec(&Old::A { x: 1 }).unwrap();
        assert!(postcard::from_bytes::<New>(&old).is_err(), "a new reader cannot read an old frame");
        let new = postcard::to_stdvec(&New::A { x: 1, name: Some("n".into()) }).unwrap();
        assert_eq!(postcard::from_bytes::<Old>(&new).unwrap(), Old::A { x: 1 }, "an old reader ignores the extra bytes");
    }

    #[test]
    fn appended_variants_round_trip() {
        let id = Identity::generate();
        let server = ServerId::from_bytes([1; 32]);
        let claim = crate::election::SignedHostClaim::sign(&id, HostClaim { server, epoch: 3, host: id.user(), prev: None, issued_ms: 9 }).unwrap();
        let att = crate::election::SignedAttestation::sign(
            &id,
            crate::election::Attestation { server, host: UserId([2; 32]), epoch: 3, member: id.user(), issued_ms: 9 },
        )
        .unwrap();
        for msg in [Ctl::Attest(att.clone()), Ctl::HostCompare2 { claim, attests: vec![att] }] {
            let f = encode_frame(&msg).unwrap();
            assert_eq!(decode_body(&f[4..]).unwrap(), msg);
        }
        assert_eq!(postcard::to_stdvec(&Ctl::Attest(crate::election::SignedAttestation {
            att: crate::election::Attestation { server, host: UserId([2; 32]), epoch: 0, member: UserId([3; 32]), issued_ms: 0 },
            sig: Sig64::ZERO
        }))
        .unwrap()[0], 36);
    }

    #[test]
    fn large_heads_are_split_and_reassembled() {
        let big = Heads((0..(HEADS_PER_FRAME * 2 + 5)).map(|i| (UserId([(i % 251) as u8; 32]), i as u64)).collect());
        let frames = sync_have_frames(big.clone());
        assert_eq!(frames.len(), 3);
        let mut asm = HeadsAssembler::default();
        let mut out = None;
        for f in frames {
            assert!(encode_frame(&f).is_ok(), "every part fits in a frame");
            let Ctl::SyncHavePart { heads, more } = f else { panic!("expected parts") };
            out = asm.push(heads, more);
        }
        assert_eq!(out, Some(big));
        let small = Heads(vec![(UserId([1; 32]), 3)]);
        assert_eq!(sync_have_frames(small.clone()), vec![Ctl::SyncHave { heads: small }]);
    }

    #[test]
    fn limits() {
        let clip = ClipInfo { hash: BlobHash([5; 32]), name: "boing".into(), size: 1000, duration_ms: 900 };
        assert!(clip.is_valid());
        assert!(!ClipInfo { name: "x".repeat(MAX_CLIP_NAME_CHARS + 1), ..clip.clone() }.is_valid());
        assert!(!ClipInfo { name: "a\u{7}b".into(), ..clip.clone() }.is_valid());
        assert!(!ClipInfo { name: String::new(), ..clip.clone() }.is_valid());
        assert!(!ClipInfo { size: MAX_CLIP_BYTES + 1, ..clip.clone() }.is_valid());
        assert!(!ClipInfo { duration_ms: u32::MAX, ..clip.clone() }.is_valid());
        // A 20 s file with decoder padding (the app stores the decoded length).
        assert!(ClipInfo { duration_ms: 20_050, ..clip.clone() }.is_valid());
        assert!(ClipInfo { duration_ms: MAX_CLIP_MS + CLIP_DURATION_TOLERANCE_MS, ..clip.clone() }.is_valid());
        assert!(!ClipInfo { duration_ms: MAX_CLIP_MS + CLIP_DURATION_TOLERANCE_MS + 1, ..clip.clone() }.is_valid());
        assert!(!Ctl::SoundPlayed { by: UserId([1; 32]), clip: ClipInfo { name: "n".repeat(500_000), ..clip.clone() } }.within_limits());
        assert!(Ctl::SoundPlayed { by: UserId([1; 32]), clip }.within_limits());
        assert!(!Ctl::Have { add: vec![BlobHash([1; 32]); MAX_HAVE_DELTA + 1], remove: vec![] }.within_limits());
        // An honest sender splits large deltas so every frame passes.
        let frames = have_frames(&vec![BlobHash([1; 32]); 3 * MAX_HAVE_DELTA + 1], &[BlobHash([2; 32])]);
        assert_eq!(frames.len(), 5);
        assert!(frames.iter().all(Ctl::within_limits));
        assert!(!Ctl::Reject { code: RejectCode::Internal, reason: "r".repeat(100_000) }.within_limits());
        assert_eq!(sanitize_text("a\u{1b}[31mb\nc", 200), "a[31mbc");
        assert_eq!(sanitize_text(&"z".repeat(500), 200).len(), 200);
        let hint = AddrHint { relay: Some("r".repeat(MAX_HINT_RELAY_BYTES + 1)), direct: vec!["127.0.0.1:1".parse().unwrap(); 20] };
        let b = hint.bounded();
        assert!(b.relay.is_none());
        assert_eq!(b.direct.len(), MAX_HINT_DIRECT);
    }
}
