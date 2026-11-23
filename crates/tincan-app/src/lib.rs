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
//! Everything below the public surface is owned by the tincan-app build agents.

use std::path::PathBuf;
use std::sync::Arc;

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
    _private: (),
}

impl App {
    /// Open the profile at `cfg.data_dir` and start networking, files and audio.
    /// Errors (user-facing string) if the profile is already open in another process
    /// (lock file) or the directory is unusable.
    pub async fn start(cfg: AppConfig) -> Result<Arc<App>, String> {
        let _ = cfg;
        todo!("tincan-app agent")
    }

    /// Run the UI command `cmd` (a `types.ts` `Commands` key) with camelCase JSON `args`.
    /// Unknown command names return an error string.
    pub async fn call(&self, cmd: &str, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let _ = (cmd, args);
        todo!("tincan-app agent")
    }

    /// Subscribe to UI events. A lagging receiver skips events; the UI recovers via
    /// `app_snapshot` on `snapshot` events, so the shell just forwards what it gets.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<UiEvent> {
        todo!("tincan-app agent")
    }

    /// Start delivering frames of screen-share `track` to `sink` (`screen_watch`).
    /// Replaces an existing sink for the same track. Stops on `screen_unwatch`, when the
    /// share ends, when leaving voice, or when `sink` returns false.
    pub async fn screen_watch(&self, track: u16, sink: VideoSink) -> Result<(), String> {
        let _ = (track, sink);
        todo!("tincan-app agent")
    }

    /// The bytes and MIME type behind `{file_url_base}{hash_hex}`, if that blob is complete
    /// locally (at most the 25 MB attachment limit; `None` otherwise). The shell serves them
    /// from an asynchronous URI scheme handler with
    /// `Cross-Origin-Resource-Policy: cross-origin` (the page runs with COEP require-corp).
    pub async fn file_for_url(&self, hash_hex: &str) -> Option<(Vec<u8>, String)> {
        let _ = hash_hex;
        todo!("tincan-app agent")
    }

    /// Clean shutdown: hand off hosted servers, stop media, close stores. Idempotent.
    pub async fn shutdown(&self) {
        todo!("tincan-app agent")
    }
}
