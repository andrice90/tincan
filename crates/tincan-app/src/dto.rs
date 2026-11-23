//! JSON shapes of the UI contract. Each type mirrors one interface of `ui/src/api/types.ts`
//! (`serde(rename_all = "camelCase")`, enum strings exactly as in the TS unions).
//!
//! Ids are strings: users z-base-32, everything else lowercase hex. Times are unix ms.

use serde::{Deserialize, Serialize};

/// `Me`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    /// z-base-32 user id.
    pub user_id: String,
    /// `"tincan1u" + userId`.
    pub user_code: String,
    /// Display name.
    pub name: String,
}

/// `HostState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostState {
    /// Looking for whoever is online.
    Connecting,
    /// Connected to a host.
    Member,
    /// This device is the host.
    Hosting,
    /// Hosting, nobody else online.
    Solo,
    /// Network down.
    Offline,
}

/// `PathKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    /// Direct UDP path.
    Direct,
    /// Relayed.
    Relay,
    /// No path.
    None,
}

/// `Connection`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    /// Role of this device.
    pub state: HostState,
    /// Current host.
    pub host_id: Option<String>,
    /// Display name of the host.
    pub host_name: Option<String>,
    /// Path to the host.
    pub path: PathKind,
    /// Round-trip time to the host.
    pub rtt_ms: Option<u32>,
}

/// `ServerSummary`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSummary {
    /// Server id (hex).
    pub id: String,
    /// Server name.
    pub name: String,
    /// Owner (z32).
    pub owner_id: String,
    /// I own it.
    pub is_owner: bool,
    /// Connection status.
    pub connection: Connection,
    /// Any channel (text or voice room chat) has unread messages.
    pub unread: bool,
    /// Members online, me included.
    pub online_count: u32,
}

/// `ChannelKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    /// Text channel.
    Text,
    /// Voice room.
    Voice,
}

/// `Channel`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    /// Channel id (hex).
    pub id: String,
    /// Server id (hex).
    pub server_id: String,
    /// Name.
    pub name: String,
    /// Text or voice.
    pub kind: ChannelKind,
    /// Sort position within its kind.
    pub position: u32,
    /// Has unread messages (a voice room's chat counts).
    pub unread: bool,
}

/// `Member`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    /// z32 user id.
    pub user_id: String,
    /// Display name.
    pub name: String,
    /// Online in the roster.
    pub online: bool,
    /// Current host.
    pub is_host: bool,
    /// Server owner.
    pub is_owner: bool,
    /// Voice room they are in.
    pub voice_channel_id: Option<String>,
    /// Mic muted.
    pub muted: bool,
    /// Deafened.
    pub deafened: bool,
    /// Sharing their screen.
    pub sharing: bool,
    /// Speaking right now.
    pub speaking: bool,
}

/// `Attachment`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    /// Blob hash (hex).
    pub hash: String,
    /// File name.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// MIME type.
    pub mime: String,
}

/// `TransferState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferState {
    /// No transfer record and not held.
    Idle,
    /// Waiting for a slot.
    Queued,
    /// Fetching.
    Downloading,
    /// Held completely.
    Complete,
    /// Nobody online has it.
    Unavailable,
    /// A local error stopped it.
    Failed,
}

impl TransferState {
    /// The string stored in the downloads table (same as the JSON value).
    pub fn as_str(self) -> &'static str {
        match self {
            TransferState::Idle => "idle",
            TransferState::Queued => "queued",
            TransferState::Downloading => "downloading",
            TransferState::Complete => "complete",
            TransferState::Unavailable => "unavailable",
            TransferState::Failed => "failed",
        }
    }
}

/// `Transfer`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transfer {
    /// Blob hash (hex).
    pub hash: String,
    /// State.
    pub state: TransferState,
    /// Bytes held.
    pub have: u64,
    /// Total size.
    pub total: u64,
    /// Online peers known to hold the complete file.
    pub peers: u32,
}

/// `Message`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    /// Event id (hex).
    pub id: String,
    /// Server or DM conversation (hex).
    pub conv_id: String,
    /// Channel (hex), null for DMs.
    pub channel_id: Option<String>,
    /// Author (z32).
    pub author_id: String,
    /// Author display name.
    pub author_name: String,
    /// Text (empty once deleted).
    pub text: String,
    /// Files (empty once deleted).
    pub attachments: Vec<Attachment>,
    /// Unix ms from the event's hlc.
    pub sent_at: u64,
    /// Deleted.
    pub deleted: bool,
    /// DMs only: not yet delivered.
    pub pending: bool,
}

/// `MessagePage`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    /// Oldest first.
    pub messages: Vec<Message>,
    /// `<hlc>:<eventIdHex>` of the oldest message when older ones exist.
    pub cursor: Option<String>,
}

/// `DmSummary`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DmSummary {
    /// DM conversation (hex).
    pub conv_id: String,
    /// The other user (z32).
    pub user_id: String,
    /// Their name.
    pub name: String,
    /// Online somewhere I can see.
    pub online: bool,
    /// Has unread messages.
    pub unread: bool,
    /// Newest message time.
    pub last_message_at: Option<u64>,
}

/// `VoiceParticipant`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceParticipant {
    /// z32 user id.
    pub user_id: String,
    /// Name.
    pub name: String,
    /// Muted.
    pub muted: bool,
    /// Deafened.
    pub deafened: bool,
    /// Speaking.
    pub speaking: bool,
    /// Screen track while sharing.
    pub screen_track: Option<u16>,
    /// How loud they play for me (0..2, saved per user; 1 for me).
    pub volume: f64,
}

/// `VoiceSession.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceKind {
    /// Server voice room.
    Room,
    /// 1:1 call.
    Call,
}

/// `VoiceSession`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceSession {
    /// Room or call.
    pub kind: VoiceKind,
    /// Server or DM conversation (hex).
    pub conv_id: String,
    /// Voice room (hex), null in calls.
    pub channel_id: Option<String>,
    /// Room or contact name.
    pub name: String,
    /// Everyone in it.
    pub participants: Vec<VoiceParticipant>,
    /// I am muted.
    pub muted: bool,
    /// I am deafened.
    pub deafened: bool,
    /// I am sharing.
    pub sharing: bool,
    /// Path to the host or peer.
    pub path: PathKind,
    /// Round-trip time.
    pub rtt_ms: Option<u32>,
}

/// `CallState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CallState {
    /// I am calling.
    RingingOut,
    /// I am being called.
    RingingIn,
    /// Both in the call.
    Active,
    /// Over.
    Ended,
}

/// `CallInfo`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallInfo {
    /// Call id (decimal string).
    pub call_id: String,
    /// The other party (z32).
    pub user_id: String,
    /// Their name.
    pub name: String,
    /// State.
    pub state: CallState,
}

/// `ScreenSource.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenSourceKind {
    /// A display.
    Display,
    /// A window.
    Window,
}

/// `ScreenSource`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenSource {
    /// `display:<n>` or `window:<hwnd>`.
    pub id: String,
    /// Display or window.
    pub kind: ScreenSourceKind,
    /// Name.
    pub name: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// `ShareMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareMode {
    /// Sharp text, up to 15 fps.
    Text,
    /// Smooth motion, up to 30 fps.
    Motion,
}

/// `Clip`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Clip {
    /// Blob hash (hex).
    pub hash: String,
    /// Name.
    pub name: String,
    /// Decoded length.
    pub duration_ms: u32,
    /// File size.
    pub size: u64,
    /// In my library.
    pub owned: bool,
    /// Who played it (heard clips).
    pub heard_from: Option<String>,
    /// Download state while pulling.
    pub transfer: Option<Transfer>,
}

/// `AudioDevice`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevice {
    /// Device id.
    pub id: String,
    /// Name.
    pub name: String,
    /// System default.
    pub is_default: bool,
}

/// `AudioDevices`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevices {
    /// Microphones.
    pub inputs: Vec<AudioDevice>,
    /// Speakers.
    pub outputs: Vec<AudioDevice>,
}

/// `Settings.theme`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow Windows.
    #[default]
    System,
    /// Dark.
    Dark,
    /// Light.
    Light,
}

/// `Settings`. Missing fields take their defaults (older stored documents, partial input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Microphone (null = system default).
    pub input_device_id: Option<String>,
    /// Speaker (null = system default).
    pub output_device_id: Option<String>,
    /// Noise suppression on.
    pub noise_suppression: bool,
    /// Echo cancellation on.
    pub echo_cancellation: bool,
    /// Input gain 0..2.
    pub input_volume: f64,
    /// Output gain 0..2.
    pub output_volume: f64,
    /// Attachments up to this size are fetched automatically (bytes, 0 = off).
    pub auto_fetch_limit: u64,
    /// Where `open_file` exports files.
    pub download_dir: String,
    /// May this device host servers.
    pub allow_hosting: bool,
    /// Custom iroh relay (null = n0's public relays).
    pub custom_relay_url: Option<String>,
    /// UI theme.
    pub theme: Theme,
}

/// `AppSnapshot`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    /// Null until a display name is picked.
    pub me: Option<Me>,
    /// Every joined server.
    pub servers: Vec<ServerSummary>,
    /// Every DM conversation.
    pub dms: Vec<DmSummary>,
    /// Settings.
    pub settings: Settings,
    /// Current voice session.
    pub voice: Option<VoiceSession>,
    /// Current call.
    pub call: Option<CallInfo>,
    /// App version.
    pub version: String,
}

// ---------------------------------------------------------------- event payloads

/// `channels-updated` payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelsUpdated {
    /// Server id (hex).
    pub server_id: String,
    /// Every visible channel.
    pub channels: Vec<Channel>,
}

/// `members-updated` payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MembersUpdated {
    /// Server id (hex).
    pub server_id: String,
    /// Every current member.
    pub members: Vec<Member>,
}

/// `typing` payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Typing {
    /// Server or DM conversation (hex).
    pub conv_id: String,
    /// Channel (hex), null for DMs.
    pub channel_id: Option<String>,
    /// Typist (z32).
    pub user_id: String,
    /// Typist name.
    pub name: String,
}

/// `speaking` payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Speaking {
    /// z32 user id.
    pub user_id: String,
    /// Speaking now.
    pub speaking: bool,
}

/// `sound-played` payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SoundPlayed {
    /// Who played it (z32).
    pub user_id: String,
    /// Their name.
    pub name: String,
    /// The clip.
    pub clip: Clip,
}

/// `mic-level` payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicLevel {
    /// 0..1.
    pub level: f32,
}

/// `toast.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToastKind {
    /// Informational.
    Info,
    /// Something failed.
    Error,
}

/// `toast` payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Toast {
    /// Info or error.
    pub kind: ToastKind,
    /// Text to show.
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enum_strings_match_types_ts() {
        assert_eq!(json!(HostState::Solo), json!("solo"));
        assert_eq!(json!(PathKind::None), json!("none"));
        assert_eq!(json!(TransferState::Unavailable), json!("unavailable"));
        assert_eq!(json!(CallState::RingingOut), json!("ringing-out"));
        assert_eq!(json!(CallState::RingingIn), json!("ringing-in"));
        assert_eq!(json!(ChannelKind::Voice), json!("voice"));
        assert_eq!(json!(ScreenSourceKind::Window), json!("window"));
        assert_eq!(json!(Theme::System), json!("system"));
        assert_eq!(json!(ToastKind::Error), json!("error"));
        assert_eq!(json!(VoiceKind::Call), json!("call"));
        for s in [
            TransferState::Idle,
            TransferState::Queued,
            TransferState::Downloading,
            TransferState::Complete,
            TransferState::Unavailable,
            TransferState::Failed,
        ] {
            assert_eq!(json!(s), json!(s.as_str()));
        }
    }

    #[test]
    fn fields_are_camel_case() {
        let m = Message {
            id: "a".into(),
            conv_id: "b".into(),
            channel_id: None,
            author_id: "c".into(),
            author_name: "d".into(),
            text: String::new(),
            attachments: vec![],
            sent_at: 1,
            deleted: false,
            pending: true,
        };
        let v = json!(m);
        for k in ["id", "convId", "channelId", "authorId", "authorName", "text", "attachments", "sentAt", "deleted", "pending"] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
        assert!(v["channelId"].is_null());
    }

    #[test]
    fn settings_accept_partial_documents() {
        let s: Settings = serde_json::from_value(json!({ "theme": "dark" })).unwrap();
        assert_eq!(s.theme, Theme::Dark);
        assert!(serde_json::from_value::<Settings>(json!({ "theme": "blue" })).is_err());
    }
}
