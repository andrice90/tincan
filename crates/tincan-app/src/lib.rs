//! tincan-app: the orchestrator. Wires tincan-store, tincan-net, tincan-files, tincan-audio
//! and tincan-screen together and exposes the UI API defined in `ui/src/api/types.ts`.
//!
//! CONTRACT (frozen; the Tauri shell in `src-tauri` is built against exactly this surface):
//!
//! - [`App::start`] opens one profile directory and starts everything.
//! - [`App::call`] runs any UI command by its `types.ts` name with the same camelCase JSON
//!   args the UI passes to `invoke`, and returns the JSON result or the user-facing error
//!   string (api.md rules, including the machine strings `no-hardware-encoder`,
//!   `not-in-voice`, `deafened`). `screen_watch` and the `window_*` commands are NOT handled
//!   here: the shell owns them (`screen_watch` goes through [`App::screen_watch`]).
//! - [`App::subscribe`] yields every UI event (`types.ts` `Events` name + JSON payload).
//!   The app never floods it: during an initial history sync it sends no per-message
//!   events and one refresh (`channels-updated` / `server-updated` / `snapshot`) when sync
//!   completes. The shell forwards every event; on `RecvError::Lagged` it re-sends the
//!   current snapshot (`call("app_snapshot")` emitted as `snapshot`), which makes the UI
//!   drop and reload its message caches.
//! - [`App::screen_watch`] / `call("screen_unwatch", ..)` deliver raw video messages
//!   (`tincan_core::media::ui_video_message` layout) to a sink.
//! - [`App::file_for_url`] (async, bytes + MIME) backs the shell's file protocol; `file_url` returns
//!   `{AppConfig::file_url_base}{hashHex}` once the blob is complete.
//! - [`App::shutdown`] performs the clean host handoff and closes stores (call on window
//!   close, await it before exiting the process).
//!
//! Everything below the public surface is owned by the tincan-app build agents. The module
//! map, the event mapping and the media boundary are described in `docs/notes-tincan-app.md`.
#![warn(missing_docs)]

mod commands;
mod app;
mod dispatch;
pub mod dto;
mod emit;
mod images;
mod index;
mod media;
mod profile;
mod pump;
mod settings;
mod shell;
mod transfers;
mod views;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::app::Core;

/// How to open a profile.
#[derive(Clone, Debug)]
pub struct AppConfig {
    /// Profile directory (created if missing). Holds the SQLite file, `identity.key`, the
    /// blob store and a lock file. The shell resolves it from `--profile` / `TINCAN_PROFILE`
    /// (architecture.md, Profiles). The WebView2 data dir is the shell's business.
    pub data_dir: PathBuf,
    /// Reported in `Status.app_version` and `Me`/snapshot where relevant.
    pub app_version: String,
    /// Prefix `file_url` puts in front of the blob hash hex, e.g.
    /// `http://tincan.localhost/file/`. The shell serves that path with `file_for_url`.
    pub file_url_base: String,
    /// Test and headless options. `TestOptions::default()` is production behaviour.
    pub test: TestOptions,
}

/// Switches used by in-process end-to-end tests (and possibly a headless smoke run).
#[derive(Clone, Debug, Default)]
pub struct TestOptions {
    /// No relays, no mDNS, no rendezvous: loopback/direct addresses only.
    pub offline_network: bool,
    /// Do not open real audio devices or capture; use null/synthetic audio IO.
    pub no_devices: bool,
}

/// One UI event: `name` is a key of the `types.ts` `Events` map, `payload` its JSON value.
#[derive(Clone, Debug)]
pub struct UiEvent {
    /// Event name, e.g. `"message-added"`.
    pub name: &'static str,
    /// Event payload, camelCase JSON matching `types.ts`.
    pub payload: serde_json::Value,
}

/// Receives raw video messages for one watched track. Return `false` to stop delivery
/// (for example when the webview channel is gone); the app then unwatches the track.
pub type VideoSink = Box<dyn FnMut(Vec<u8>) -> bool + Send + 'static>;

/// A running profile. Cheap to share (`Arc<App>`); all methods take `&self`.
pub struct App {
    core: Arc<Core>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "App({:?})", self.core) }
}

impl App {
    /// Open the profile at `cfg.data_dir` and start networking, files and audio.
    /// Errors (user-facing string) if the profile is already open in another process
    /// (lock file) or the directory is unusable.
    pub async fn start(cfg: AppConfig) -> Result<Arc<App>, String> {
        let core = Core::start(cfg).await?;
        Ok(Arc::new(App { core }))
    }

    /// Run the UI command `cmd` (a `types.ts` `Commands` key) with camelCase JSON `args`.
    /// Unknown command names return an error string.
    pub async fn call(&self, cmd: &str, args: serde_json::Value) -> Result<serde_json::Value, String> {
        dispatch::call(&self.core, cmd, args).await
    }

    /// Subscribe to UI events. A lagging receiver skips events; the UI recovers via
    /// `app_snapshot` on `snapshot` events, so the shell just forwards what it gets.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<UiEvent> { self.core.events.subscribe() }

    /// Start delivering frames of screen-share `track` to `sink` (`screen_watch`).
    /// Replaces an existing sink for the same track. Stops on `screen_unwatch`, when the
    /// share ends, when leaving voice, or when `sink` returns false.
    pub async fn screen_watch(&self, track: u16, sink: VideoSink) -> Result<(), String> {
        if self.core.is_closed() {
            return Err("Tincan is shutting down.".into());
        }
        self.core.media.screen_watch(&self.core, track, sink).await
    }

    /// The bytes and MIME type behind `{file_url_base}{hash_hex}`, if that blob is complete
    /// locally (at most the 25 MB attachment limit; `None` otherwise). The shell serves them
    /// from an asynchronous URI scheme handler with
    /// `Cross-Origin-Resource-Policy: cross-origin` (the page runs with COEP require-corp).
    pub async fn file_for_url(&self, hash_hex: &str) -> Option<(Vec<u8>, String)> {
        if self.core.is_closed() {
            return None;
        }
        self.core.file_for_url(hash_hex).await
    }

    /// Clean shutdown: hand off hosted servers, stop media, close stores. Idempotent.
    pub async fn shutdown(&self) { self.core.shutdown().await }

    // ------------------------------------------------------------------ test hooks

    /// Test hook: every command name `call` handles (`types.ts` keys).
    #[doc(hidden)]
    pub fn handled_commands() -> Vec<&'static str> {
        dispatch::CORE_COMMANDS.iter().chain(media::COMMANDS.iter()).copied().collect()
    }

    /// Test hook: every event name the app can emit.
    #[doc(hidden)]
    pub fn event_names() -> &'static [&'static str] { &emit::EVENT_NAMES }

    /// Test hook: loopback socket addresses of this instance (relays and discovery are off
    /// with `TestOptions::offline_network`).
    #[doc(hidden)]
    pub fn test_addrs(&self) -> Vec<SocketAddr> { self.core.loopback_addrs() }

    /// Test hook: tell this instance where `user_id` (z-base-32) can be reached, and retry
    /// anything waiting for that user (DM outbox, cold open).
    #[doc(hidden)]
    pub fn test_add_addr(&self, user_id: &str, addrs: &[SocketAddr]) -> Result<(), String> {
        let user = commands::parse_user(user_id)?;
        let hint = tincan_core::wire::AddrHint { relay: None, direct: addrs.to_vec() };
        self.core.net.add_addr_hint(user, hint);
        Ok(())
    }

    /// Test hook (`TestOptions::no_devices`): the synthetic microphone sends speech-like
    /// bursts (`true`) or silence (`false`, the default).
    #[doc(hidden)]
    pub fn test_synthetic_voice(&self, on: bool) { self.core.media.test_synthetic_voice(on) }

    /// Test hook (`TestOptions::no_devices`): the synthetic microphone panics on its next
    /// read, which kills the audio engine thread (the app must restart it).
    #[doc(hidden)]
    pub fn test_crash_audio_engine(&self) { self.core.media.test_crash_audio() }

    /// Test hook: announce in my voice session that I played a clip with this hash, name,
    /// size and length, without holding it or checking anything, the way a modified client
    /// could.
    #[doc(hidden)]
    pub async fn test_sound_played(&self, hash_hex: &str, name: &str, size: u64, duration_ms: u32) -> Result<(), String> {
        let hash = transfers::parse_hash(hash_hex)?;
        let clip = tincan_core::wire::ClipInfo { hash, name: name.to_string(), size, duration_ms };
        self.core.net.sound_played(clip).await.map_err(app::net_err)
    }

    /// Test hook: starting call audio fails while `on`, as if the audio engine could not
    /// start (voice rooms and calls refuse to attach).
    #[doc(hidden)]
    pub fn test_fail_audio_start(&self, on: bool) { self.core.media.test_fail_audio_start(on) }

    /// Test hook: allow real screen capture although `TestOptions::no_devices` is set
    /// (audio stays synthetic). For the hardware screen-share test only.
    #[doc(hidden)]
    pub fn test_allow_screen_capture(&self) { self.core.media.test_allow_capture() }

    /// Test hook: media counters, `{ audioIn, videoOut, engineRestarts, engineRunning }`.
    #[doc(hidden)]
    pub fn test_media_counters(&self) -> serde_json::Value { self.core.media.test_counters() }

    /// Test hook: `true` makes the event pump drop every file event, as if its receiver had
    /// lagged; `false` resumes it and runs the lag recovery (`RecvError::Lagged`).
    #[doc(hidden)]
    pub fn test_pause_file_events(&self, paused: bool) {
        self.core.test_files_paused.store(paused, std::sync::atomic::Ordering::Relaxed);
        if !paused {
            self.core.on_files_lagged();
        }
    }

    /// Test hook: fetch the blob `hash_hex` straight from `user_id` at `addrs` over the
    /// blobs protocol, the way a modified client could (no message, no index, no holder
    /// lookup). True if the whole blob arrived within `secs`.
    #[doc(hidden)]
    pub async fn test_fetch_from(&self, hash_hex: &str, user_id: &str, addrs: &[SocketAddr], secs: u64) -> bool {
        self.core.test_fetch_from(hash_hex, user_id, addrs, secs).await
    }

    /// Test hook: publish a message in `conv_id` (with `channel_id` for servers) whose one
    /// attachment claims `hash_hex`, `name` and `size`, without holding the bytes, the way
    /// a modified client could.
    #[doc(hidden)]
    pub async fn test_post_attachment_ref(&self, conv_id: &str, channel_id: Option<&str>, hash_hex: &str, name: &str, size: u64) -> Result<(), String> {
        self.core.test_post_attachment_ref(conv_id, channel_id, hash_hex, name, size).await
    }

    /// Test hook: publish a `ProfileImage` in `server_id` naming `hash_hex` (declared PNG of
    /// `size` bytes) without holding it, the way a modified client could.
    #[doc(hidden)]
    pub async fn test_post_profile_image(&self, server_id: &str, hash_hex: &str, size: u64) -> Result<(), String> {
        let server = commands::parse_conv(server_id)?;
        let hash = transfers::parse_hash(hash_hex)?;
        let image = tincan_core::event::ImageRef { hash, size, mime: "image/png".into() };
        self.core.net.publish(server, tincan_core::event::Body::ProfileImage { image: Some(image) }).await.map_err(app::net_err)?;
        Ok(())
    }
}
