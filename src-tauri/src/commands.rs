//! Tauri commands. Every UI command goes through [`api`] (see `ui/src/api/client.ts`);
//! `screen_watch` carries a Channel and the window commands are the shell's own.

use std::sync::atomic::Ordering;

use serde_json::{Value, json};
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, State, WebviewWindow};
use tincan_app::VideoSink;

use crate::Shell;

/// Runs the `types.ts` command `cmd` with its camelCase `args` in tincan-app. Waits while the
/// App is still starting. Errors are the app's user-facing strings.
#[tauri::command]
pub async fn api(shell: State<'_, Shell>, cmd: String, args: Option<Value>) -> Result<Value, String> {
    let app = shell.gate.get().await?;
    app.call(&cmd, args.unwrap_or_else(|| json!({}))).await
}

/// Starts delivering screen-share `track` to `on_frame` as raw `ArrayBuffer`s
/// (`tincan_core::media::ui_video_message` layout). Delivery stops when the app unwatches the
/// track, when a send fails, or when the page that asked for it has been reloaded.
#[tauri::command]
pub async fn screen_watch(shell: State<'_, Shell>, track: u16, on_frame: Channel<InvokeResponseBody>) -> Result<(), String> {
    let pages = shell.page_loads.clone();
    let page = pages.load(Ordering::Acquire);
    let app = shell.gate.get().await?;
    let sink: VideoSink = Box::new(move |msg| {
        pages.load(Ordering::Acquire) == page && on_frame.send(InvokeResponseBody::Raw(msg)).is_ok()
    });
    app.screen_watch(track, sink).await
}

/// Minimizes the window (title bar button).
#[tauri::command]
pub fn window_minimize(window: WebviewWindow) -> Result<(), String> {
    window.minimize().map_err(|e| window_error("minimize", e))
}

/// Maximizes the window, or restores it when it is maximized (title bar button).
#[tauri::command]
pub fn window_toggle_maximize(window: WebviewWindow) -> Result<(), String> {
    let maximized = window.is_maximized().map_err(|e| window_error("maximize", e))?;
    if maximized { window.unmaximize() } else { window.maximize() }.map_err(|e| window_error("maximize", e))
}

/// Closes the app the same way as Alt+F4: hide, shut down cleanly (at most 5 s), exit.
#[tauri::command]
pub fn window_close(app: AppHandle) {
    crate::begin_close(&app);
}

fn window_error(what: &str, e: tauri::Error) -> String {
    eprintln!("tincan: window {what} failed: {e}");
    format!("Couldn't {what} the window. Try again.")
}
