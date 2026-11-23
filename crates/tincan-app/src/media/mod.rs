//! Media: voice rooms, 1:1 calls, screen share, soundboard, audio devices, mic test and
//! per-user volume.
//!
//! - [`engine`]: the one `tincan_audio::AudioEngine` of this profile (created on first use),
//!   its event forwarder, and recovery when its thread dies.
//! - [`session`]: which `tincan_net::MediaSession` the engine is attached to. Attach and
//!   detach are driven by one `reconcile` over `Net::voice_info` / `Net::call_info`, because
//!   network events and command results race. Also the `VoiceSession` / `CallInfo` DTOs,
//!   mute, deafen, per-user volume, devices and the mic test.
//! - [`clips`]: the soundboard library (store `clips` table plus `BlobKind::Clip` blobs),
//!   heard clips, pulling a heard clip and announcing the library to hosts.
//! - [`screen`]: sources, sharing (tincan-screen, or a synthetic pattern under
//!   `TestOptions::no_devices`) and watching (`App::screen_watch`).
//!
//! Lock order: `st`, then `engine`, then `watchers`. Nothing here calls `Core::refresh` or
//! `Core::snapshot` while holding `st` (those call back into [`Media::is_speaking`] and
//! [`Media::voice_snapshot`]).

mod clips;
mod engine;
mod screen;
mod session;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use tincan_core::{BlobHash, UserId};
use tincan_files::FileEvent;
use tincan_net::NetEvent;

use crate::app::Core;
use crate::commands::{parse_channel, parse_conv, parse_user};
use crate::transfers::parse_hash;
use crate::{AppConfig, VideoSink, dto};

/// Every media command name (`types.ts` keys) routed to [`Media::call`].
pub(crate) const COMMANDS: &[&str] = &[
    "voice_join",
    "voice_leave",
    "call_start",
    "call_accept",
    "call_decline",
    "call_hangup",
    "set_muted",
    "set_deafened",
    "set_user_volume",
    "screen_sources",
    "screen_start",
    "screen_stop",
    "screen_unwatch",
    "clips",
    "clip_add",
    "clip_remove",
    "clip_rename",
    "clip_play",
    "clip_fetch",
    "audio_devices",
    "mic_test",
];

/// Largest soundboard clip file (architecture.md: 5 MB; the UI's MB is 1024²). tincan-files
/// enforces the same cap on clip downloads.
pub(crate) const MAX_CLIP_BYTES: u64 = tincan_files::MAX_CLIP_BYTES;
/// Longest soundboard clip (decoded length).
pub(crate) const MAX_CLIP_MS: u32 = 20_000;
/// Longest clip name (the UI's `CLIP_NAME_MAX`).
pub(crate) const MAX_CLIP_NAME_CHARS: usize = 32;

/// Counters behind `App::test_media_counters`.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// Audio datagrams handed to the engine.
    pub audio_in: AtomicU64,
    /// Video messages delivered to `screen_watch` sinks.
    pub video_out: AtomicU64,
    /// Times the audio engine was restarted after its thread died.
    pub engine_restarts: AtomicU64,
}

/// Mutable media state (one lock; see the module docs for the order).
#[derive(Default)]
struct State {
    /// Mic muted (the UI's state: deafen sets it, unmute clears deafen).
    muted: bool,
    /// Deafened.
    deafened: bool,
    /// The session the engine is attached to.
    attached: Option<session::Attached>,
    /// My ringing-out call: its session waits here until the call is active.
    pending_call: Option<session::PendingCall>,
    /// Session-changing commands in flight (reconcile does not detach meanwhile).
    switching: u32,
    /// Users speaking right now in my session (me included).
    speaking: HashSet<UserId>,
    /// Short ids and members of my session.
    room: session::RoomMap,
    /// Per-user playback volume (0..2), persisted.
    volumes: HashMap<UserId, f32>,
    /// My running screen share.
    share: Option<screen::ActiveShare>,
    /// Clips heard in this session.
    heard: clips::Heard,
    /// When each player's last `SoundPlayed` went through (cleared on detach).
    sound_seen: HashMap<UserId, std::time::Instant>,
    /// Heard clips being pulled into the library (persisted, survives a restart).
    fetching: HashMap<BlobHash, clips::Fetch>,
    /// Decoded clips (bounded, dropped on leaving voice).
    pcm: clips::PcmCache,
    /// The mic test is on.
    mic_test: bool,
    /// Last `voice-updated` payload sent.
    last_voice: Option<Value>,
    /// The call as last reported (peer, state), for end-of-call notices.
    last_call: Option<(UserId, tincan_net::CallState)>,
}

/// Media state of one profile.
pub(crate) struct Media {
    /// Use null/synthetic audio IO and no capture (tests, headless).
    no_devices: bool,
    /// Serializes session-changing commands (join, leave, calls, share start/stop).
    ops: tokio::sync::Mutex<()>,
    st: Mutex<State>,
    engine: Arc<Mutex<engine::EngineSlot>>,
    /// `screen_watch` sinks by track.
    watchers: Arc<Mutex<HashMap<u16, VideoSink>>>,
    /// Synthetic audio switches (no_devices only).
    test: Arc<engine::TestAudio>,
    counters: Arc<Counters>,
    /// `tincan_screen::hardware_encoder_available`, asked once.
    hw_encoder: tokio::sync::OnceCell<bool>,
    /// Test hook: real screen capture allowed despite `no_devices`.
    allow_capture: AtomicBool,
    /// Shutdown started.
    closed: AtomicBool,
    /// Ids for sessions and shares.
    next_id: AtomicU64,
}

impl std::fmt::Debug for Media {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Media").field("no_devices", &self.no_devices).finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VoiceJoinArgs {
    server_id: String,
    channel_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserArgs {
    user_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallArgs {
    call_id: String,
}

#[derive(Deserialize)]
struct MutedArgs {
    muted: bool,
}

#[derive(Deserialize)]
struct DeafenedArgs {
    deafened: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VolumeArgs {
    user_id: String,
    volume: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScreenStartArgs {
    source_id: String,
    mode: dto::ShareMode,
}

#[derive(Deserialize)]
struct TrackArgs {
    track: u16,
}

#[derive(Deserialize)]
struct HashArgs {
    hash: String,
}

#[derive(Deserialize)]
struct ClipAddArgs {
    path: String,
    name: String,
}

#[derive(Deserialize)]
struct ClipRenameArgs {
    hash: String,
    name: String,
}

#[derive(Deserialize)]
struct MicTestArgs {
    on: bool,
}

fn parse<T: serde::de::DeserializeOwned>(cmd: &str, args: Value) -> Result<T, String> {
    crate::dispatch::parse_args(cmd, args)
}

fn ok<T: serde::Serialize>(v: T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| {
        tracing::error!(error = %e, "encoding a media result failed");
        "Something went wrong. Try again.".to_string()
    })
}

impl Media {
    /// Media state for a profile opened with `cfg`. Nothing runs until first use.
    pub(crate) fn new(cfg: &AppConfig) -> Self {
        Self {
            no_devices: cfg.test.no_devices,
            ops: tokio::sync::Mutex::new(()),
            st: Mutex::new(State { last_voice: Some(Value::Null), ..State::default() }),
            engine: Arc::new(Mutex::new(engine::EngineSlot::default())),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            test: Arc::new(engine::TestAudio::default()),
            counters: Arc::new(Counters::default()),
            hw_encoder: tokio::sync::OnceCell::new(),
            allow_capture: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
        }
    }

    /// After `Core` exists: load persisted volumes and pending clip pulls, announce the
    /// clip library to every host.
    pub(crate) fn start(&self, core: &Arc<Core>) {
        {
            let mut st = self.st.lock();
            st.volumes = session::load_volumes(core);
            st.fetching = clips::load_fetches(core);
        }
        self.announce_clips(core);
    }

    fn is_closed(&self) -> bool { self.closed.load(Ordering::Acquire) }

    fn next_id(&self) -> u64 { self.next_id.fetch_add(1, Ordering::Relaxed) }

    /// Run a media command (one of [`COMMANDS`]).
    pub(crate) async fn call(&self, core: &Arc<Core>, cmd: &str, args: Value) -> Result<Value, String> {
        match cmd {
            "voice_join" => {
                let a: VoiceJoinArgs = parse(cmd, args)?;
                let server = parse_conv(&a.server_id)?;
                let channel = parse_channel(&a.channel_id)?;
                ok(self.voice_join(core, server, channel).await?)
            }
            "voice_leave" => {
                self.voice_leave(core).await;
                Ok(Value::Null)
            }
            "call_start" => {
                let a: UserArgs = parse(cmd, args)?;
                ok(self.call_start(core, parse_user(&a.user_id)?).await?)
            }
            "call_accept" => {
                let a: CallArgs = parse(cmd, args)?;
                ok(self.call_accept(core, &a.call_id).await?)
            }
            "call_decline" => {
                let a: CallArgs = parse(cmd, args)?;
                self.call_decline(core, &a.call_id).await;
                Ok(Value::Null)
            }
            "call_hangup" => {
                self.call_hangup(core).await;
                Ok(Value::Null)
            }
            "set_muted" => {
                let a: MutedArgs = parse(cmd, args)?;
                self.set_muted(core, a.muted);
                Ok(Value::Null)
            }
            "set_deafened" => {
                let a: DeafenedArgs = parse(cmd, args)?;
                self.set_deafened(core, a.deafened);
                Ok(Value::Null)
            }
            "set_user_volume" => {
                let a: VolumeArgs = parse(cmd, args)?;
                self.set_user_volume(core, parse_user(&a.user_id)?, a.volume)?;
                Ok(Value::Null)
            }
            "screen_sources" => ok(self.screen_sources().await?),
            "screen_start" => {
                let a: ScreenStartArgs = parse(cmd, args)?;
                ok(self.screen_start(core, &a.source_id, a.mode).await?)
            }
            "screen_stop" => {
                self.screen_stop(core).await;
                Ok(Value::Null)
            }
            "screen_unwatch" => {
                let a: TrackArgs = parse(cmd, args)?;
                self.screen_unwatch(core, a.track);
                Ok(Value::Null)
            }
            "clips" => ok(self.clip_list(core)),
            "clip_add" => {
                let a: ClipAddArgs = parse(cmd, args)?;
                ok(self.clip_add(core, &a.path, &a.name).await?)
            }
            "clip_remove" => {
                let a: HashArgs = parse(cmd, args)?;
                self.clip_remove(core, parse_hash(&a.hash)?).await;
                Ok(Value::Null)
            }
            "clip_rename" => {
                let a: ClipRenameArgs = parse(cmd, args)?;
                self.clip_rename(core, parse_hash(&a.hash)?, &a.name)?;
                Ok(Value::Null)
            }
            "clip_play" => {
                let a: HashArgs = parse(cmd, args)?;
                self.clip_play(core, parse_hash(&a.hash)?).await?;
                Ok(Value::Null)
            }
            "clip_fetch" => {
                let a: HashArgs = parse(cmd, args)?;
                ok(self.clip_fetch(core, parse_hash(&a.hash)?).await?)
            }
            "audio_devices" => ok(self.audio_devices().await?),
            "mic_test" => {
                let a: MicTestArgs = parse(cmd, args)?;
                self.mic_test(core, a.on)?;
                Ok(Value::Null)
            }
            other => Err(format!("Unknown command: {other}")),
        }
    }

    /// `App::screen_watch`.
    pub(crate) async fn screen_watch(&self, core: &Arc<Core>, track: u16, sink: VideoSink) -> Result<(), String> {
        self.watch(core, track, sink)
    }

    /// Media-related network events: `VoiceUpdated`, `KeyframeRequested`, `TargetBitrate`,
    /// `SoundPlayed`, `LossReport`, `CallInvite`, `CallUpdated`, plus `RosterUpdated` and
    /// `ConnectionChanged` (also handled by the pump). Never blocks.
    pub(crate) fn on_net_event(&self, core: &Arc<Core>, ev: &NetEvent) {
        if self.is_closed() {
            return;
        }
        match ev {
            NetEvent::VoiceUpdated { .. } => self.reconcile(core),
            NetEvent::KeyframeRequested { .. } => {
                if let Some(s) = &self.st.lock().share {
                    s.request_keyframe();
                }
            }
            NetEvent::TargetBitrate { bps, .. } => {
                if let Some(s) = &self.st.lock().share {
                    s.set_bitrate(*bps);
                }
            }
            NetEvent::LossReport { pct } => {
                if let Some(e) = self.engine_handle() {
                    e.set_loss_hint(*pct);
                }
            }
            NetEvent::SoundPlayed { conv, by, clip } => self.on_sound(core, *conv, *by, clip),
            NetEvent::CallUpdated { call } => self.on_call_updated(core, call),
            NetEvent::RosterUpdated { server, .. } => self.on_roster(core, *server),
            NetEvent::ConnectionChanged { server, .. } => self.on_connection(core, *server),
            _ => {}
        }
    }

    /// The network event stream lagged: re-read everything (a missed channel change may
    /// have deleted my voice room).
    pub(crate) fn on_lagged(&self, core: &Arc<Core>) {
        if !self.is_closed() {
            self.reconcile(core);
            let server = self.st.lock().attached.as_ref().and_then(|a| match a.kind {
                session::Kind::Room { server, .. } => Some(server),
                session::Kind::Call { .. } => None,
            });
            if let Some(server) = server {
                self.on_channels(core, server);
            }
        }
    }

    /// Every file event (clip downloads progress and complete here).
    pub(crate) fn on_file_event(&self, core: &Arc<Core>, ev: &FileEvent) {
        if self.is_closed() {
            return;
        }
        match ev {
            FileEvent::Transfer(info) if info.kind == tincan_files::BlobKind::Clip => self.on_clip_transfer(core, info),
            FileEvent::Completed { hash, kind: tincan_files::BlobKind::Clip, .. } => self.on_clip_completed(core, *hash),
            _ => {}
        }
    }

    /// The file event stream lagged: a pull may have completed unseen. Finish every pending
    /// pull whose blob is complete now, and refresh the clip list.
    pub(crate) fn on_files_lagged(&self, core: &Arc<Core>) {
        if self.is_closed() {
            return;
        }
        let done: Vec<BlobHash> = self.st.lock().fetching.keys().copied().filter(|h| core.files.is_complete(h)).collect();
        for hash in done {
            self.on_clip_completed(core, hash);
        }
        let st = self.st.lock();
        self.emit_clips_locked(core, &st);
    }

    /// Settings changed: apply devices, volumes and audio processing to the engine live.
    pub(crate) fn apply_settings(&self, _core: &Arc<Core>, settings: &dto::Settings) {
        if let Some(e) = self.engine_handle() {
            engine::apply_settings(&e, settings, self.no_devices);
        }
    }

    /// `AppSnapshot.voice`.
    pub(crate) fn voice_snapshot(&self, core: &Core) -> Option<dto::VoiceSession> {
        let st = self.st.lock();
        self.voice_dto(core, &st)
    }

    /// `AppSnapshot.call`.
    pub(crate) fn call_snapshot(&self, core: &Core) -> Option<dto::CallInfo> {
        core.net.call_info().and_then(|c| session::call_dto(core, &c))
    }

    /// `Member.speaking`.
    pub(crate) fn is_speaking(&self, user: &UserId) -> bool { self.st.lock().speaking.contains(user) }

    /// Stop calls, rooms, shares and the audio engine (before the network shuts down).
    pub(crate) async fn shutdown(&self, core: &Arc<Core>) {
        self.closed.store(true, Ordering::Release);
        let share = {
            let mut st = self.st.lock();
            st.pending_call = None;
            let share = st.share.take();
            self.detach_locked(core, &mut st);
            share
        };
        if let Some(s) = share {
            core.net.share_stop();
            s.stop().await;
        }
        self.watchers.lock().clear();
        let engine = self.engine.lock().engine.take();
        if let Some(e) = engine {
            // Dropping the last handle joins the engine thread (a few ms while devices close).
            let _ = tokio::task::spawn_blocking(move || drop(e)).await;
        }
    }

    // ------------------------------------------------------------------ test hooks

    /// Test hook: the synthetic microphone (no_devices) sends speech-like bursts.
    pub(crate) fn test_synthetic_voice(&self, on: bool) { self.test.voice.store(on, Ordering::Release); }

    /// Test hook: the synthetic microphone panics on its next read, which kills the engine
    /// thread the way a panic in the decoder would.
    pub(crate) fn test_crash_audio(&self) { self.test.crash.store(true, Ordering::Release); }

    /// Test hook: starting call audio fails while `on` (an engine that cannot start).
    pub(crate) fn test_fail_audio_start(&self, on: bool) { self.test.fail_start.store(on, Ordering::Release); }

    /// Test hook: allow real screen capture despite `no_devices` (audio stays null).
    pub(crate) fn test_allow_capture(&self) { self.allow_capture.store(true, Ordering::Release); }

    /// Test hook: media counters as JSON.
    pub(crate) fn test_counters(&self) -> Value {
        let c = &self.counters;
        json!({
            "audioIn": c.audio_in.load(Ordering::Relaxed),
            "videoOut": c.video_out.load(Ordering::Relaxed),
            "engineRestarts": c.engine_restarts.load(Ordering::Relaxed),
            "engineRunning": self.engine.lock().running().is_some(),
        })
    }
}
