//! Voice rooms, screen share and 1:1 calls (part B): the host SFU, member media links and
//! call signaling (docs/architecture.md "Media", "Host SFU", "Direct messages and 1:1 calls").
//!
//! - Audio rides QUIC datagrams on the server connection (member <-> host) or on the DM
//!   connection (calls). The host forwards each datagram verbatim to the other members of the
//!   sender's voice room and to its own engine, and injects its own engine's packets the same
//!   way. The sender's header gets the current track id and host-epoch byte written by this
//!   layer, so the app never restarts its engine after a migration.
//! - Video goes as one QUIC uni stream per GOP: `VideoStreamHeader`, then frames. Every hop
//!   (sharer -> host, host -> viewer, call peer -> peer) runs the same forwarder
//!   ([`track::VideoTrack`]): a bounded queue per viewer, reset-and-skip-to-keyframe when a
//!   viewer falls behind, coalesced keyframe requests (at most one per second per track), a
//!   GOP cache for instant start, and an AIMD bitrate target fed by drops and receiver
//!   reports.
//! - A [`MediaSession`] lives until `voice_leave` (or the call ends). Its channels stay the
//!   same across host migrations: on every new transport the session re-sends `VoiceJoin`,
//!   its `Subscribe`s and its share.

mod api;
mod hub;
mod recv;
mod sfu;
mod stats;
mod track;
mod voice;
mod wire;

use bytes::Bytes;
use tincan_core::media::VideoFrame;
use tincan_core::{ChannelId, ConvId, UserId};
use tokio::sync::mpsc;

pub(crate) use hub::{HostMedia, MediaHub, MemberMedia};

/// Capacity of [`MediaSession::audio_out`] in packets (20 ms each: about 1.3 s).
pub const AUDIO_OUT_QUEUE: usize = 64;
/// Capacity of [`MediaSession::audio_in`] in packets. Packets are dropped when it is full.
pub const AUDIO_IN_QUEUE: usize = 256;
/// Capacity of [`MediaSession::video_in`] in frames. When it is full, the track skips to its
/// next keyframe (and one is requested).
pub const VIDEO_IN_QUEUE: usize = 64;
/// Capacity of the frame channel returned by `Net::share_start`.
pub const SHARE_QUEUE: usize = 8;

/// Highest bitrate target the host gives a screen share.
pub const VIDEO_MAX_BPS: u32 = 4_500_000;
/// Lowest bitrate target.
pub const VIDEO_MIN_BPS: u32 = 300_000;
/// Cap when a viewer is reached only through a relay.
pub const RELAYED_VIEWER_BPS: u32 = 800_000;
/// Cap when the sharer is reached only through a relay.
pub const RELAYED_SHARER_BPS: u32 = 1_000_000;
/// Host upload budget for video, split evenly over all viewer streams.
pub const HOST_UPLINK_BUDGET_BPS: u32 = 10_000_000;

/// The local side of a voice room or a 1:1 call. Created by `Net::voice_join`,
/// `Net::call_start` or `Net::call_accept`; ends with `Net::voice_leave` / `Net::call_hangup`
/// (or when the call ends remotely). The channels stay valid across host migrations.
pub struct MediaSession {
    /// The server (room) or DM conversation (call).
    pub conv: ConvId,
    /// My voice track id when the session started (`track_id(short_id, KIND_VOICE)`). Pass it
    /// to `AudioEngine::start_call`. Outgoing packets are re-stamped with the current track id,
    /// so it does not matter if it changes later.
    pub local_track: u16,
    /// Low byte of the host epoch when the session started (0 in calls).
    pub epoch: u8,
    /// Encoded audio datagrams from the app's engine (`AudioEngine::start_call(.., out)`).
    pub audio_out: mpsc::Sender<Bytes>,
    /// Remote audio datagrams, for `AudioEngine::push_packet`.
    pub audio_in: mpsc::Receiver<Bytes>,
    /// Frames of the video tracks I subscribed to (`Net::subscribe_track`), in order, each
    /// track starting at a keyframe.
    pub video_in: mpsc::Receiver<(u16, VideoFrame)>,
}

impl std::fmt::Debug for MediaSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaSession").field("conv", &self.conv).field("local_track", &self.local_track).field("epoch", &self.epoch).finish()
    }
}

/// State of my current voice session (`Net::voice_info`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VoiceInfo {
    /// The server (room) or DM conversation (call).
    pub conv: ConvId,
    /// The voice room (None in a call).
    pub channel: Option<ChannelId>,
    /// The other party (calls only).
    pub peer: Option<UserId>,
    /// My current voice track id.
    pub local_track: u16,
    /// Current host-epoch low byte (0 in calls).
    pub epoch: u8,
    /// Media is flowing (connected to the host, hosting, or the call is active).
    pub connected: bool,
    /// The path to the host or peer is relayed (raise the jitter buffer: `set_relayed`).
    pub relayed: bool,
    /// My screen-share track id while sharing.
    pub sharing: Option<u16>,
    /// Video tracks I subscribed to.
    pub watching: Vec<u16>,
}

/// Where a 1:1 call stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallState {
    /// I called; the other side is ringing.
    RingingOut,
    /// The other side called; I am ringing.
    RingingIn,
    /// Both sides are in the call.
    Active,
    /// The call is over (see [`CallInfo::ended`]).
    Ended,
}

/// Why a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallEnd {
    /// Someone hung up.
    Hangup,
    /// The callee declined (or was busy in another call).
    Declined,
    /// Nobody answered within the ringing timeout (45 s).
    NoAnswer,
    /// The caller gave up before I answered.
    Cancelled,
    /// The connection to the other side was lost.
    Lost,
}

/// A 1:1 call as the UI shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallInfo {
    /// Random call id.
    pub call_id: u64,
    /// The other party.
    pub peer: UserId,
    /// The DM conversation with them.
    pub conv: ConvId,
    /// Current state.
    pub state: CallState,
    /// Why it ended (state `Ended` only).
    pub ended: Option<CallEnd>,
    /// The other side's microphone is muted.
    pub peer_muted: bool,
    /// The other side is deafened.
    pub peer_deafened: bool,
    /// The other side's screen-share track while they share.
    pub peer_sharing: Option<u16>,
}

/// Short ids in a call (no host hands them out): the lower user id takes 0.
pub(crate) fn call_short_id(me: &UserId, peer: &UserId) -> u16 { if me < peer { 0 } else { 1 } }
