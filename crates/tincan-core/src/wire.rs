//! Control messages and stream framing for `tincan/server/1` and `tincan/dm/1`.
//!
//! Frame = `u32 little-endian length || postcard(Ctl)`, length <= MAX_FRAME.
//! Append new `Ctl` variants only at the end.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::election::{SignedHostClaim, Status};
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
    }
}
