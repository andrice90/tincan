//! Events emitted by the network layer and connection status types.

use std::sync::Arc;

use tincan_core::event::StoredEvent;
use tincan_core::state::Change;
use tincan_core::wire::{ClipInfo, Roster};
use tincan_core::{ChannelId, ConvId, ServerId, UserId};

use crate::media::CallInfo;

/// Role of this device in a server session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ConnState {
    /// Looking for whoever is online (cold open, failover, following a moved host).
    #[default]
    Connecting,
    /// Connected to a host.
    Member,
    /// This device is the host and at least one member is connected.
    Hosting,
    /// This device is the host and nobody else is online.
    Solo,
    /// Nobody reachable and this device may not host (allow_hosting off), or the session ended.
    Offline,
}

/// Kind of the selected network path to the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PathKind {
    /// Direct UDP (hole punched or LAN).
    Direct,
    /// Through an iroh relay.
    Relay,
    /// No path (hosting, solo, connecting).
    #[default]
    None,
}

/// Connection status of one server, as shown in the UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// What this device is doing for the server.
    pub state: ConnState,
    /// The current host (this device when hosting or solo).
    pub host: Option<UserId>,
    /// Selected path to the host (members only).
    pub path: PathKind,
    /// Round-trip time to the host (members only).
    pub rtt_ms: Option<u32>,
    /// Current host epoch (0 = never had a host).
    pub epoch: u64,
}

/// Something the app should react to. Delivered on a `tokio::sync::broadcast` channel
/// (`Net::subscribe`); a lagging receiver gets `RecvError::Lagged` and should refresh its
/// view from `Net::conv_state` / storage.
#[derive(Clone, Debug)]
pub enum NetEvent {
    /// A non-message change to a conversation (rename, channels, members, invites,
    /// deletion). Emitted in addition to `EventAdded`.
    ConvUpdated {
        /// The conversation.
        conv: ConvId,
        /// What changed.
        change: Change,
    },
    /// An event was validated, applied and stored (live, sync, or authored locally).
    EventAdded {
        /// The conversation.
        conv: ConvId,
        /// The event (shared: the broadcast channel clones events per receiver).
        event: Arc<StoredEvent>,
        /// What it changed.
        change: Change,
    },
    /// The roster of a server changed (members online, voice, mute, sharing, succession).
    RosterUpdated {
        /// The server.
        server: ServerId,
        /// The new roster.
        roster: Roster,
    },
    /// The connection status of a server changed.
    ConnectionChanged {
        /// The server.
        server: ServerId,
        /// New status.
        info: ConnectionInfo,
    },
    /// Someone is typing.
    Typing {
        /// Server or DM conversation.
        conv: ConvId,
        /// Channel (the DM channel for DMs).
        channel: ChannelId,
        /// The typist.
        user: UserId,
    },
    /// The DM peer confirmed my events up to this seq (messages with seq <= it are delivered).
    DmDelivered {
        /// The DM conversation.
        conv: ConvId,
        /// Highest confirmed seq of my events.
        up_to_seq: u64,
    },
    /// A DM conversation record was created (by `dm_open` or an incoming DM).
    DmOpened {
        /// The DM conversation.
        conv: ConvId,
        /// The other user.
        peer: UserId,
    },
    /// This device joined a server (after `join` or `create_server`).
    Joined {
        /// The server.
        server: ServerId,
    },
    /// This device left a server (after `leave`); its local data is gone.
    Left {
        /// The server.
        server: ServerId,
    },
    /// The owner removed me from a server. Its session has stopped; the local copy stays
    /// readable until `join` with a current invite (or `leave`).
    Removed {
        /// The server.
        server: ServerId,
    },
    /// Host side: a member's connection came up (part B: start forwarding media for it).
    MemberLinkUp {
        /// The server.
        server: ServerId,
        /// The member.
        user: UserId,
        /// Its short id (media track ids derive from it).
        short_id: u16,
    },
    /// Host side: a member's connection went away.
    MemberLinkDown {
        /// The server.
        server: ServerId,
        /// The member.
        user: UserId,
    },
    /// A DM connection to `peer` came up (part B: 1:1 call media rides on it).
    DmLinkUp {
        /// The other user.
        peer: UserId,
    },
    /// The DM connection to `peer` went away.
    DmLinkDown {
        /// The other user.
        peer: UserId,
    },
    /// Something went wrong that the user may want to know about.
    Error {
        /// Related conversation, if any.
        conv: Option<ConvId>,
        /// User-presentable text.
        message: String,
    },
    /// My voice session changed (connected, migrated, sharing, watching, ended): read
    /// `Net::voice_info`. `conv` is the room's server or the call's DM conversation.
    VoiceUpdated {
        /// The session's conversation.
        conv: ConvId,
    },
    /// Someone needs a keyframe of my screen share: call `ScreenShare::request_keyframe`.
    /// Already coalesced to at most one per second per hop.
    KeyframeRequested {
        /// My screen track.
        track: u16,
    },
    /// New bitrate target for my screen share: call `ScreenShare::set_bitrate`.
    TargetBitrate {
        /// My screen track.
        track: u16,
        /// Bits per second.
        bps: u32,
    },
    /// Someone in my room (or call) played a soundboard clip.
    SoundPlayed {
        /// Server or DM conversation.
        conv: ConvId,
        /// Who played it (fetch the clip from them, `who_has` for more holders).
        by: UserId,
        /// The clip.
        clip: ClipInfo,
    },
    /// Loss of my outgoing audio as the others receive it (worst receiver), in percent: call
    /// `AudioEngine::set_loss_hint`. Emitted when it changes.
    LossReport {
        /// Percent lost.
        pct: u8,
    },
    /// An incoming call started ringing (also announced as `CallUpdated`).
    CallInvite {
        /// The call.
        call: CallInfo,
    },
    /// A call changed state (ringing, active, the peer's mute/share state, ended).
    CallUpdated {
        /// The call (`state == Ended` once it is over; `Net::call_info` is then None).
        call: CallInfo,
    },
    /// `author` signed two different events with the same `seq` in `conv` (a modified
    /// client). Replicas may show either version for that seq; both signed events are kept
    /// as a proof. Show a warning on that member.
    Equivocation {
        /// Server or DM conversation.
        conv: ConvId,
        /// The equivocating author.
        author: UserId,
        /// The seq both events carry.
        seq: u64,
    },
    /// The initial history sync of `conv` finished, once per connection:
    /// - servers: the host's history after every (re)connect to a host (also after a host
    ///   migration). Never while this device hosts or is solo.
    /// - DMs: the contact's history after every DM connect; at once when they had nothing I
    ///   lacked.
    ///
    /// Every event that sync applied was emitted as `EventAdded` before this.
    SyncComplete {
        /// Server or DM conversation.
        conv: ConvId,
    },
    /// A DM contact announced a new display name over the DM connection. Also readable with
    /// `Net::dm_peer_name`. Checked like profile names (1-100 characters after trimming, no
    /// control characters) but still text from a peer.
    DmNameChanged {
        /// The DM conversation.
        conv: ConvId,
        /// The contact.
        peer: UserId,
        /// The name they announced.
        name: String,
    },
}
