//! Forwards `App::subscribe()` to the webview as Tauri events (`types.ts` `Events`).
//!
//! On `RecvError::Lagged` the forwarder emits `snapshot` with the current `app_snapshot`
//! (the UI then drops and reloads its message caches) and goes on with the events still
//! buffered. Those carry full objects in order, so the last one per object wins and the UI
//! ends up at the latest state.

use std::future::Future;
use std::sync::{Arc, Weak};

use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Runtime};
use tincan_app::{App, UiEvent};
use tokio::sync::broadcast::{self, error::RecvError};

/// Event name of the full state refresh.
pub const SNAPSHOT: &str = "snapshot";

/// Runs until the event stream closes. `emit` sends one event to the UI; `snapshot` fetches
/// the current `app_snapshot` after a lag.
pub async fn forward<E, S, F>(mut rx: broadcast::Receiver<UiEvent>, mut emit: E, mut snapshot: S)
where
    E: FnMut(&str, Value),
    S: FnMut() -> F,
    F: Future<Output = Result<Value, String>>,
{
    loop {
        match rx.recv().await {
            Ok(ev) => emit(ev.name, ev.payload),
            Err(RecvError::Lagged(skipped)) => match snapshot().await {
                Ok(snap) => emit(SNAPSHOT, snap),
                Err(e) => eprintln!("tincan: {skipped} events skipped and the snapshot failed: {e}"),
            },
            Err(RecvError::Closed) => return,
        }
    }
}

/// Subscribes to `app` and forwards to every webview of `handle` on the async runtime.
/// Call before releasing the readiness gate so no event after start-up is missed.
pub fn spawn_forwarder<R: Runtime>(app: &Arc<App>, handle: AppHandle<R>) {
    let rx = app.subscribe();
    let weak: Weak<App> = Arc::downgrade(app);
    let emit = move |name: &str, payload: Value| {
        if let Err(e) = handle.emit(name, payload) {
            eprintln!("tincan: emitting {name} failed: {e}");
        }
    };
    let snapshot = move || {
        let app = weak.upgrade();
        async move {
            match app {
                Some(app) => app.call("app_snapshot", json!({})).await,
                None => Err("Tincan stopped.".to_string()),
            }
        }
    };
    tauri::async_runtime::spawn(forward(rx, emit, snapshot));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &'static str, n: u64) -> UiEvent {
        UiEvent { name, payload: json!({ "n": n }) }
    }

    #[tokio::test]
    async fn forwards_in_order_until_closed() {
        let (tx, rx) = broadcast::channel(16);
        for n in 0..3 {
            tx.send(ev("toast", n)).unwrap();
        }
        drop(tx);
        let mut seen = Vec::new();
        forward(rx, |name, p| seen.push((name.to_string(), p)), || async { Err("unused".to_string()) }).await;
        let names: Vec<_> = seen.iter().map(|(n, p)| format!("{n}{}", p["n"])).collect();
        assert_eq!(names, ["toast0", "toast1", "toast2"]);
    }

    #[tokio::test]
    async fn lag_emits_a_snapshot_then_the_buffered_events() {
        let (tx, rx) = broadcast::channel(4);
        for n in 0..10 {
            tx.send(ev("message-added", n)).unwrap();
        }
        drop(tx);
        let mut seen = Vec::new();
        let mut calls = 0;
        forward(
            rx,
            |name, p| seen.push(format!("{name}{}", p["n"])),
            || {
                calls += 1;
                async { Ok(json!({ "n": "snap" })) }
            },
        )
        .await;
        assert_eq!(calls, 1);
        assert_eq!(seen, ["snapshot\"snap\"", "message-added6", "message-added7", "message-added8", "message-added9"]);
    }

    #[tokio::test]
    async fn failed_snapshot_keeps_forwarding() {
        let (tx, rx) = broadcast::channel(2);
        for n in 0..5 {
            tx.send(ev("toast", n)).unwrap();
        }
        drop(tx);
        let mut seen = Vec::new();
        forward(rx, |name, _| seen.push(name.to_string()), || async { Err("down".to_string()) }).await;
        assert_eq!(seen, ["toast", "toast"]);
    }
}
