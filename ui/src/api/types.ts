// Contract between the Rust app (src-tauri) and the UI.
// Rust DTOs in src-tauri/src/dto.rs mirror these exactly (serde rename_all = "camelCase").
// Ids are strings: users are z-base-32 (52 chars), everything else lowercase hex (64 chars).
// Times are unix milliseconds (number).

export type UserId = string;
export type ServerId = string;
export type ChannelId = string;
export type MessageId = string;
export type BlobHash = string;
/** A server id or a DM conversation id. */
export type ConvId = string;

export interface Me {
  userId: UserId;
  /** "tincan1u" + userId: the full ID, behind "Copy full ID" (people normally share the friend code). */
  userCode: string;
  name: string;
  /** My avatar (an `http://tincan.localhost/file/<hash>` URL), null when I have none. */
  avatar: string | null;
}

export type HostState =
  | "connecting" // looking for whoever is online
  | "member" // connected to a host
  | "hosting" // this device is the host
  | "solo" // hosting, nobody else online
  | "offline"; // network down

export type PathKind = "direct" | "relay" | "none";

export interface Connection {
  state: HostState;
  hostId: UserId | null;
  hostName: string | null;
  path: PathKind;
  rttMs: number | null;
}

export interface ServerSummary {
  id: ServerId;
  name: string;
  ownerId: UserId;
  isOwner: boolean;
  connection: Connection;
  /** Any channel is unread, a voice room's chat included. */
  unread: boolean;
  /** Members currently online (including me). */
  onlineCount: number;
  /** Server icon URL once this device holds and checked it; null otherwise. */
  icon: string | null;
  /**
   * The owner removed me. The server stays readable up to the removal, offline and
   * read-only, until I join again with a current invite (or leave it).
   */
  removed: boolean;
}

export type ChannelKind = "text" | "voice";

export interface Channel {
  id: ChannelId;
  serverId: ServerId;
  name: string;
  kind: ChannelKind;
  position: number;
  /** Has unread messages. A voice room has a chat too, so this applies to both kinds. */
  unread: boolean;
}

export interface Member {
  userId: UserId;
  name: string;
  online: boolean;
  isHost: boolean;
  isOwner: boolean;
  /** Voice room this member is in, if any. */
  voiceChannelId: ChannelId | null;
  muted: boolean;
  deafened: boolean;
  sharing: boolean;
  speaking: boolean;
  /** Avatar URL once this device holds and checked it; null shows the coloured initial. */
  avatar: string | null;
}

export interface Attachment {
  hash: BlobHash;
  name: string;
  size: number;
  mime: string;
}

export type TransferState = "idle" | "queued" | "downloading" | "complete" | "unavailable" | "failed";

export interface Transfer {
  hash: BlobHash;
  state: TransferState;
  have: number;
  total: number;
  /** Online peers known to hold the complete file. */
  peers: number;
}

export interface Message {
  id: MessageId;
  convId: ConvId;
  /** Null for DMs. */
  channelId: ChannelId | null;
  authorId: UserId;
  authorName: string;
  text: string;
  attachments: Attachment[];
  sentAt: number;
  deleted: boolean;
  /** DMs only: not yet delivered to the other side. */
  pending: boolean;
}

export interface MessagePage {
  messages: Message[]; // oldest first
  /** Pass as `before` to load older messages; null when at the start. */
  cursor: string | null;
}

export interface DmSummary {
  convId: ConvId;
  userId: UserId;
  name: string;
  online: boolean;
  unread: boolean;
  lastMessageAt: number | null;
  /** Their avatar URL (announced over the DM or from a shared server); null when none yet. */
  avatar: string | null;
}

export interface VoiceParticipant {
  userId: UserId;
  name: string;
  muted: boolean;
  deafened: boolean;
  speaking: boolean;
  /** Avatar URL once this device holds and checked it; null shows the coloured initial. */
  avatar: string | null;
  /** Present while they share their screen. */
  screenTrack: number | null;
  /** How loud they are for me (0..2, saved per user; 1 for me). Their clips follow it too. */
  volume: number;
  /** How loud their soundboard clips are for me, on top of `volume` (0..2, saved per user; 1 for me). */
  soundboardVolume: number;
  /** How loud their screen share's sound is for me (0..2, saved per sharer; 1 for me). */
  shareVolume: number;
}

/** `user_volume`: both saved volumes of a person (1 when never changed). */
export interface UserVolume {
  volume: number;
  soundboardVolume: number;
}

/**
 * What my screen share sends besides the picture: nothing, the shared window's app, or
 * everything the PC plays except Tincan (a shared display).
 */
export type ShareAudio = "off" | "app" | "system";

export interface VoiceSession {
  /** Server voice room or DM call. */
  kind: "room" | "call";
  convId: ConvId;
  channelId: ChannelId | null;
  name: string;
  participants: VoiceParticipant[];
  muted: boolean;
  deafened: boolean;
  sharing: boolean;
  /** The sound my share sends ("off" when not sharing, turned off, or not capturable). */
  shareAudio: ShareAudio;
  path: PathKind;
  rttMs: number | null;
  /** Hashes of my clips playing now: from `clip_play` until they end or `clip_stop`. */
  playingClips: BlobHash[];
}

export type CallState = "ringing-out" | "ringing-in" | "active" | "ended";

export interface CallInfo {
  callId: string;
  userId: UserId;
  name: string;
  state: CallState;
}

export interface ScreenSource {
  id: string;
  kind: "display" | "window";
  name: string;
  width: number;
  height: number;
}

export type ShareMode = "text" | "motion";

export interface Clip {
  hash: BlobHash;
  name: string;
  durationMs: number;
  size: number;
  /** In my library (I can play it and I seed it). */
  owned: boolean;
  /** For "heard" clips: who played it. */
  heardFrom: UserId | null;
  transfer: Transfer | null;
}

export interface AudioDevice {
  id: string;
  name: string;
  isDefault: boolean;
}

export interface AudioDevices {
  inputs: AudioDevice[];
  outputs: AudioDevice[];
  /** This build has the AI noise suppression model. */
  aiNoiseSuppressionAvailable: boolean;
}

export interface Settings {
  inputDeviceId: string | null;
  outputDeviceId: string | null;
  noiseSuppression: boolean;
  /** DPDFNet instead of WebRTC noise suppression (when `AudioDevices.aiNoiseSuppressionAvailable`). */
  aiNoiseSuppression: boolean;
  echoCancellation: boolean;
  inputVolume: number; // 0..2
  outputVolume: number; // 0..2
  /** Attachments up to this size are fetched automatically (bytes, 0 = off). */
  autoFetchLimit: number;
  downloadDir: string;
  allowHosting: boolean;
  customRelayUrl: string | null;
  theme: "system" | "dark" | "light";
}

export interface AppSnapshot {
  /** Null until the user picked a display name. */
  me: Me | null;
  servers: ServerSummary[];
  dms: DmSummary[];
  settings: Settings;
  voice: VoiceSession | null;
  call: CallInfo | null;
  version: string;
}

// ---------------------------------------------------------------- commands
// invoke(name, args) -> result. Errors reject with a string that is safe to show.

export interface Commands {
  app_snapshot: { args: {}; result: AppSnapshot };
  set_name: { args: { name: string }; result: Me };
  /**
   * My avatar: PNG or WebP bytes (at most 64 KB, at most 256x256; the UI sends 128x128),
   * or null to remove it. The app checks the bytes again and publishes the image.
   */
  set_avatar: { args: { bytes: number[] | null }; result: Me };

  create_server: { args: { name: string }; result: ServerSummary };
  /** Full invite ("tincan1...") valid for 7 days, unlimited uses: the fallback to the short code. */
  create_invite: { args: { serverId: ServerId }; result: string };
  /**
   * The server's short invite code ("ABC-DEF"), valid until the owner rotates it. For the
   * owner it is made first when there is none yet (can take a moment). Other members get null
   * until the owner has made one.
   */
  get_invite_code: { args: { serverId: ServerId }; result: string | null };
  /** Owner only: a new invite code. The old one stops working for new joins at once. */
  rotate_invite_code: { args: { serverId: ServerId }; result: string };
  /** Accepts the 6-character code (any case, dash and spaces optional) or a full invite or link. */
  join_server: { args: { invite: string }; result: ServerSummary };
  leave_server: { args: { serverId: ServerId }; result: null };
  /**
   * Owner only: remove a member. They are disconnected at once and can come back only
   * with an invite code that still works (removal revokes no invite).
   */
  kick_member: { args: { serverId: ServerId; userId: UserId }; result: null };
  rename_server: { args: { serverId: ServerId; name: string }; result: null };
  /** Owner only. Same bytes rules as `set_avatar`; null removes the icon. */
  set_server_icon: { args: { serverId: ServerId; bytes: number[] | null }; result: ServerSummary };

  channels: { args: { serverId: ServerId }; result: Channel[] };
  create_channel: { args: { serverId: ServerId; name: string; kind: ChannelKind }; result: Channel };
  rename_channel: { args: { serverId: ServerId; channelId: ChannelId; name: string }; result: null };
  delete_channel: { args: { serverId: ServerId; channelId: ChannelId }; result: null };
  members: { args: { serverId: ServerId }; result: Member[] };

  /** channelId null for DMs. before = cursor from a previous page. */
  messages: { args: { convId: ConvId; channelId: ChannelId | null; before: string | null; limit: number }; result: MessagePage };
  /** Attachments are absolute file paths picked with the dialog plugin. */
  send_message: { args: { convId: ConvId; channelId: ChannelId | null; text: string; attachments: string[] }; result: Message };
  delete_message: { args: { convId: ConvId; messageId: MessageId }; result: null };
  mark_read: { args: { convId: ConvId; channelId: ChannelId | null }; result: null };
  /** channelId null for DMs. */
  typing: { args: { convId: ConvId; channelId: ChannelId | null }; result: null };

  transfer: { args: { hash: BlobHash }; result: Transfer };
  download: { args: { hash: BlobHash }; result: Transfer };
  /** Exports to the download folder if needed and opens it with the shell. */
  open_file: { args: { hash: BlobHash; name: string }; result: null };
  /** For image previews: a URL the webview can load (same-origin asset protocol). */
  file_url: { args: { hash: BlobHash; name: string }; result: string | null };

  open_dm: { args: { userId: UserId }; result: DmSummary };
  /**
   * The DM on screen, or null when none. The app keeps the connection to that contact up
   * (presence, calls) until another DM or null replaces it.
   */
  dm_watch: { args: { convId: ConvId | null }; result: null };
  /** Accepts a friend code ("ABC-DEF", any case, dash optional), "tincan1u..." or a bare user id. */
  add_contact: { args: { code: string }; result: DmSummary };
  /** My friend code ("ABC-DEF"); made on first call. */
  get_friend_code: { args: {}; result: string };
  /** A new friend code; the old one stops resolving. */
  rotate_friend_code: { args: {}; result: string };

  voice_join: { args: { serverId: ServerId; channelId: ChannelId }; result: VoiceSession };
  voice_leave: { args: {}; result: null };
  call_start: { args: { userId: UserId }; result: CallInfo };
  call_accept: { args: { callId: string }; result: VoiceSession };
  call_decline: { args: { callId: string }; result: null };
  call_hangup: { args: {}; result: null };
  set_muted: { args: { muted: boolean }; result: null };
  set_deafened: { args: { deafened: boolean }; result: null };
  set_user_volume: { args: { userId: UserId; volume: number }; result: null };
  /** How loud someone's soundboard clips are for me, on top of their volume (0..2, saved). */
  set_user_soundboard_volume: { args: { userId: UserId; volume: number }; result: null };
  /** Both saved volumes of anyone, also outside voice (for the member menu). */
  user_volume: { args: { userId: UserId }; result: UserVolume };
  /** The soundboard volume of a voice room: a local multiplier on every clip I hear there (0..2, saved). */
  room_soundboard_volume: { args: { channelId: ChannelId }; result: number };
  set_room_soundboard_volume: { args: { channelId: ChannelId; volume: number }; result: null };
  /** How loud `userId`'s screen share sound plays (0..2), saved per sharer. */
  set_share_volume: { args: { userId: UserId; volume: number }; result: null };

  screen_sources: { args: {}; result: ScreenSource[] };
  /**
   * Error string "no-hardware-encoder" when unsupported on this machine. `audio` shares the
   * window's app sound, or for a display everything except Tincan (absent: no sound). When
   * the sound can't be captured the share starts anyway and a toast says so.
   */
  screen_start: { args: { sourceId: string; mode: ShareMode; audio?: boolean }; result: number };
  screen_stop: { args: {}; result: null };
  /**
   * Start receiving a screen share. `onFrame` is a Tauri Channel<ArrayBuffer>; each message is
   * [u8 flags(bit0 key)][u16 BE track][u32 BE ts 90kHz][H.264 Annex-B access unit].
   */
  screen_watch: { args: { track: number; onFrame: unknown }; result: null };
  screen_unwatch: { args: { track: number }; result: null };

  clips: { args: {}; result: Clip[] };
  clip_add: { args: { path: string; name: string }; result: Clip };
  clip_remove: { args: { hash: BlobHash }; result: null };
  clip_rename: { args: { hash: BlobHash; name: string }; result: null };
  clip_play: { args: { hash: BlobHash }; result: null };
  /** Stops my plays of the clip, here and in the call, at once. */
  clip_stop: { args: { hash: BlobHash }; result: null };
  /** Pull a heard clip into my library. */
  clip_fetch: { args: { hash: BlobHash }; result: Clip };

  settings_set: { args: { settings: Settings }; result: Settings };
  audio_devices: { args: {}; result: AudioDevices };
  /** While true, `mic-level` events fire about 20 times per second. */
  mic_test: { args: { on: boolean }; result: null };
  window_minimize: { args: {}; result: null };
  window_toggle_maximize: { args: {}; result: null };
  window_close: { args: {}; result: null };
}

// ---------------------------------------------------------------- events
// listen(name, payload). The UI refetches with commands when it needs full lists.

export interface Events {
  "snapshot": AppSnapshot; // after big changes (server joined/left, name set)
  "server-updated": ServerSummary;
  "channels-updated": { serverId: ServerId; channels: Channel[] };
  "members-updated": { serverId: ServerId; members: Member[] };
  "message-added": Message;
  "message-updated": Message; // deleted or delivered (pending -> false)
  "typing": { convId: ConvId; channelId: ChannelId | null; userId: UserId; name: string }; // channelId null for DMs
  "dm-updated": DmSummary;
  "voice-updated": VoiceSession | null;
  "speaking": { userId: UserId; speaking: boolean };
  "call-updated": CallInfo | null;
  "transfer-updated": Transfer;
  "clips-updated": Clip[];
  "sound-played": { userId: UserId; name: string; clip: Clip };
  "mic-level": { level: number }; // 0..1
  "toast": { kind: "info" | "error"; text: string };
}

export type CommandName = keyof Commands;
export type EventName = keyof Events;
