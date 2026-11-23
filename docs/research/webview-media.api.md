All code below is in C:/Users/andri/AppData/Local/Temp/claude/d3bbc0386af2202e/d39a4a52-4271-48fa-9546-1005d34914ef/scratchpad/research/tauri-webview2-media/ (src-tauri/ and dist/index.html). The last `cargo check` and the `cargo build --release` were both clean with zero warnings on Rust 1.99.1 MSVC. Resolved versions: tauri 2.12.7, tauri-runtime-wry 2.12.5, wry 0.57.3, webview2-com 0.39.1, windows-core 0.62.2, tokio-tungstenite 0.30.0.

What actually ran:
- (a) raw-body invoke: compiled and run.
- (b) Channel raw stream: compiled and run.
- SharedBuffer: compiled and run.
- (c) permission handlers, both the Tauri API and the raw webview2-com path: compiled only. getUserMedia and getDisplayMedia were never called, so the handlers never fired.

===== src-tauri/Cargo.toml =====
```toml
[package]
name = "tincan-media-probe"
version = "0.1.0"
edition = "2021"

[lib]
name = "probe_lib"
crate-type = ["rlib"]

[build-dependencies]
tauri-build = { version = "2.7.6", features = [] }

[dependencies]
# "custom-protocol" = production asset serving (what `tauri build` turns on).
tauri = { version = "2.12.7", features = ["custom-protocol"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["net", "rt-multi-thread", "macros", "time", "sync"] }
tokio-tungstenite = "0.30"
futures-util = "0.3"

[target.'cfg(windows)'.dependencies]
# Must match the major/minor that tauri/wry use (0.39 for tauri 2.12.x / wry 0.57.x).
webview2-com = "0.39"
windows-core = "0.62"

[profile.release]
opt-level = 3
```
build.rs: `fn main() { tauri_build::build() }`. main.rs: `fn main() { probe_lib::run() }`. icons/icon.ico must exist or tauri-build fails on Windows.

===== src-tauri/tauri.conf.json =====
```json
{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "tincan-media-probe",
  "version": "0.1.0",
  "identifier": "app.tincan.probe",
  "build": { "frontendDist": "../dist" },
  "app": {
    "withGlobalTauri": true,
    "windows": [],
    "security": {
      "csp": null,
      "headers": {
        "Cross-Origin-Opener-Policy": "same-origin",
        "Cross-Origin-Embedder-Policy": "require-corp"
      }
    }
  },
  "bundle": { "active": false, "icon": ["icons/icon.ico"] }
}
```
capabilities/default.json: `{"identifier":"default","windows":["main"],"permissions":["core:default"]}`. App commands need no ACL entries unless build.rs declares an AppManifest.

===== src-tauri/src/lib.rs (current file, verbatim) =====
```rust
//! Tincan media/IPC probe for Tauri 2.12.x on Windows (WebView2).
//!
//! (a) `push_packet` / `echo_raw`: JS -> Rust raw binary body (`tauri::ipc::Request`, `InvokeBody::Raw`).
//! (b) `start_stream`: Rust -> JS raw bytes over `tauri::ipc::Channel<InvokeResponseBody>`.
//! (c) WebView2 media permissions auto-granted two ways:
//!     1. first-class `tauri::Builder::on_permission_request` (added in tauri 2.12.0, PR #14865),
//!     2. raw `Webview::with_webview` + webview2-com `ICoreWebView2::add_PermissionRequested`,
//!        plus `ICoreWebView2_27::add_ScreenCaptureStarting` for getDisplayMedia.
//! Also a localhost WebSocket echo server to compare IPC paths.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::ipc::{Channel, InvokeBody, InvokeResponseBody, Request, Response};
use tauri::webview::{PermissionKind, PermissionResponse};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

static RX_BYTES: AtomicU64 = AtomicU64::new(0);
static SCREEN_CAPTURE_EVENTS: AtomicU64 = AtomicU64::new(0);

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

// ---------------------------------------------------------------- (a) JS -> Rust raw body
// JS: await invoke('push_packet', u8 /* Uint8Array or ArrayBuffer */, { headers: { 'x-stream': '7' } })
#[tauri::command]
fn push_packet(request: Request<'_>) -> Result<u64, String> {
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected raw (application/octet-stream) body".into());
    };
    let _stream_id: u32 = request
        .headers()
        .get("x-stream")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    RX_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    Ok(bytes.len() as u64)
}

// JS: const buf /* ArrayBuffer */ = await invoke('echo_raw', u8)
#[tauri::command]
fn echo_raw(request: Request<'_>) -> Response {
    match request.body() {
        InvokeBody::Raw(bytes) => Response::new(bytes.clone()), // InvokeResponseBody::Raw -> ArrayBuffer in JS
        InvokeBody::Json(_) => Response::new(Vec::<u8>::new()),
    }
}

// ---------------------------------------------------------------- (b) Rust -> JS raw stream
// JS: const ch = new Channel(); ch.onmessage = (buf /* ArrayBuffer */) => {...};
//     await invoke('start_stream', { size: 30000, count: 300, intervalUs: 0, onPacket: ch })
#[tauri::command]
async fn start_stream(
    size: usize,
    count: u32,
    interval_us: u64,
    on_packet: Channel<InvokeResponseBody>,
) -> Result<(), String> {
    let size = size.max(16);
    tauri::async_runtime::spawn(async move {
        for seq in 0..count {
            let mut buf = vec![0xABu8; size];
            buf[0..8].copy_from_slice(&now_us().to_le_bytes()); // send timestamp (epoch us)
            buf[8..12].copy_from_slice(&seq.to_le_bytes());
            if on_packet.send(InvokeResponseBody::Raw(buf)).is_err() {
                break;
            }
            if interval_us > 0 {
                tokio::time::sleep(Duration::from_micros(interval_us)).await;
            }
        }
    });
    Ok(())
}

// Zero-copy-ish Rust -> JS for big frames (e.g. 1080p BGRA from Windows Graphics Capture):
// WebView2 SharedBuffer (ICoreWebView2Environment12::CreateSharedBuffer + ICoreWebView2_17::PostSharedBufferToScript).
// JS: chrome.webview.addEventListener('sharedbufferreceived', e => { const ab = e.getBuffer(); e.additionalData; chrome.webview.releaseBuffer(ab); })
#[tauri::command]
fn post_shared_buffer(window: tauri::WebviewWindow, size: u64, seq: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        let sent_us = now_us();
        window
            .with_webview(move |pw| unsafe {
                use webview2_com::Microsoft::Web::WebView2::Win32::{
                    ICoreWebView2Environment12, ICoreWebView2_17,
                    COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_ONLY,
                };
                use windows_core::{Interface, HSTRING, PCWSTR};
                let Ok(env12) = pw.environment().cast::<ICoreWebView2Environment12>() else { return };
                let Ok(buf) = env12.CreateSharedBuffer(size) else { return };
                let mut ptr: *mut u8 = std::ptr::null_mut();
                if buf.Buffer(&mut ptr).is_err() || ptr.is_null() { return }
                std::ptr::write_bytes(ptr, (seq & 0xff) as u8, size as usize);
                let Ok(core) = pw.controller().CoreWebView2() else { return };
                let Ok(core17) = core.cast::<ICoreWebView2_17>() else { return };
                let json = HSTRING::from(format!("{{\"seq\":{seq},\"sentUs\":{sent_us}}}"));
                let _ = core17.PostSharedBufferToScript(
                    &buf,
                    COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_ONLY,
                    PCWSTR(json.as_ptr()),
                );
                // `buf` is released here; the script keeps its own reference until releaseBuffer().
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn now_epoch_us() -> u64 {
    now_us()
}

#[tauri::command]
fn screen_capture_events() -> u64 {
    SCREEN_CAPTURE_EVENTS.load(Ordering::Relaxed)
}

#[tauri::command]
fn report(app: AppHandle, json: String) -> Result<(), String> {
    let out = std::env::var("PROBE_OUT").unwrap_or_else(|_| "probe-report.json".into());
    std::fs::write(&out, json).map_err(|e| e.to_string())?;
    println!("PROBE_REPORT_WRITTEN {out}");
    if std::env::var("PROBE_STAY").is_err() {
        app.exit(0);
    }
    Ok(())
}

#[tauri::command]
fn set_minimized(app: AppHandle, minimized: bool) -> Result<(), String> {
    let w = app.get_webview_window("main").ok_or("no main window")?;
    if minimized { w.minimize() } else { w.unminimize() }.map_err(|e| e.to_string())
}

#[tauri::command]
fn set_hidden(app: AppHandle, hidden: bool) -> Result<(), String> {
    let w = app.get_webview_window("main").ok_or("no main window")?;
    if hidden { w.hide() } else { w.show() }.map_err(|e| e.to_string())
}

#[tauri::command]
fn ws_port(state: tauri::State<'_, WsPort>) -> u16 {
    state.0
}

struct WsPort(u16);

// ---------------------------------------------------------------- localhost WebSocket echo (comparison path)
async fn run_ws_echo(listener: tokio::net::TcpListener) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    while let Ok((tcp, _)) = listener.accept().await {
        let _ = tcp.set_nodelay(true);
        tokio::spawn(async move {
            let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else { return };
            while let Some(Ok(msg)) = ws.next().await {
                match msg {
                    Message::Binary(b) => {
                        if ws.send(Message::Binary(b)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });
    }
}

// ---------------------------------------------------------------- (c) WebView2 permission plumbing
#[cfg(windows)]
fn install_webview2_media_handlers(webview: &tauri::Webview) -> tauri::Result<()> {
    webview.with_webview(|platform_webview| unsafe {
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            ICoreWebView2, ICoreWebView2PermissionRequestedEventArgs3, ICoreWebView2_27,
            COREWEBVIEW2_PERMISSION_KIND, COREWEBVIEW2_PERMISSION_KIND_CAMERA,
            COREWEBVIEW2_PERMISSION_KIND_MICROPHONE, COREWEBVIEW2_PERMISSION_STATE_ALLOW,
        };
        use webview2_com::{PermissionRequestedEventHandler, ScreenCaptureStartingEventHandler};
        use windows_core::Interface;

        let controller = platform_webview.controller();
        let core: ICoreWebView2 = match controller.CoreWebView2() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("CoreWebView2() failed: {e:?}");
                return;
            }
        };

        // getUserMedia({audio}) / ({video}) -> allow without the WebView2 prompt.
        let mut token = 0i64;
        let handler = PermissionRequestedEventHandler::create(Box::new(|_sender, args| {
            let Some(args) = args else { return Ok(()) };
            let mut kind = COREWEBVIEW2_PERMISSION_KIND::default();
            args.PermissionKind(&mut kind)?;
            if kind == COREWEBVIEW2_PERMISSION_KIND_MICROPHONE
                || kind == COREWEBVIEW2_PERMISSION_KIND_CAMERA
            {
                args.SetState(COREWEBVIEW2_PERMISSION_STATE_ALLOW)?;
                // Optional (SDK 1.0.1661+ / ICoreWebView2PermissionRequestedEventArgs3):
                // don't persist the decision in the profile.
                if let Ok(args3) = args.cast::<ICoreWebView2PermissionRequestedEventArgs3>() {
                    let _ = args3.SetSavesInProfile(false);
                }
            }
            Ok(())
        }));
        if let Err(e) = core.add_PermissionRequested(&handler, &mut token) {
            eprintln!("add_PermissionRequested failed: {e:?}");
        }

        // getDisplayMedia(): WebView2 has NO permission kind for it; instead it raises
        // ScreenCaptureStarting (ICoreWebView2_27). We can only Cancel or let it continue;
        // the picker UI is still WebView2's own.
        match core.cast::<ICoreWebView2_27>() {
            Ok(core27) => {
                let mut token2 = 0i64;
                let sc = ScreenCaptureStartingEventHandler::create(Box::new(|_sender, args| {
                    let Some(args) = args else { return Ok(()) };
                    SCREEN_CAPTURE_EVENTS.fetch_add(1, Ordering::Relaxed);
                    args.SetCancel(false)?; // true = reject getDisplayMedia() with NotAllowedError
                    Ok(())
                }));
                if let Err(e) = core27.add_ScreenCaptureStarting(&sc, &mut token2) {
                    eprintln!("add_ScreenCaptureStarting failed: {e:?}");
                }
            }
            Err(e) => eprintln!("ICoreWebView2_27 not available (old runtime): {e:?}"),
        }
    })
}

pub fn run() {
    let data_dir: PathBuf = std::env::var("PROBE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("tincan-probe-webview"));
    let extra_args = std::env::var("PROBE_BROWSER_ARGS").unwrap_or_default();
    let query = std::env::var("PROBE_QUERY").unwrap_or_default();

    tauri::Builder::default()
        // (c1) First-class Tauri API: one handler for every webview in the app.
        .on_permission_request(|_webview, kind| match kind {
            PermissionKind::Microphone | PermissionKind::Camera => PermissionResponse::Allow,
            _ => PermissionResponse::Default,
        })
        .invoke_handler(tauri::generate_handler![
            push_packet,
            echo_raw,
            start_stream,
            now_epoch_us,
            screen_capture_events,
            report,
            set_minimized,
            set_hidden,
            post_shared_buffer,
            ws_port
        ])
        .setup(move |app| {
            // WebSocket echo server on an ephemeral 127.0.0.1 port.
            let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            std_listener.set_nonblocking(true)?;
            let port = std_listener.local_addr()?.port();
            app.manage(WsPort(port));
            tauri::async_runtime::spawn(async move {
                let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
                run_ws_echo(listener).await;
            });

            let url = if query.is_empty() {
                "index.html".to_string()
            } else {
                format!("index.html?{query}")
            };
            // wry's default args are replaced when you set additional_browser_args, so re-add them.
            let browser_args = format!(
                "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection {extra_args}"
            );
            let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::App(url.into()))
                .title("tincan probe")
                .inner_size(640.0, 400.0)
                .data_directory(data_dir.clone())
                .additional_browser_args(browser_args.trim())
                .build()?;

            #[cfg(windows)]
            install_webview2_media_handlers(window.as_ref())?;
            let _ = window;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

===== JS call shapes exercised at runtime (withGlobalTauri; with @tauri-apps/api use `import { invoke, Channel } from '@tauri-apps/api/core'`) =====
```js
const { invoke, Channel } = window.__TAURI__.core;
// JS -> Rust raw: body goes as application/octet-stream; headers are readable in Rust via request.headers()
await invoke('push_packet', new Uint8Array(200), { headers: { 'x-stream': '1' } });
// raw response -> ArrayBuffer (verified instanceof ArrayBuffer)
const ab = await invoke('echo_raw', new Uint8Array(30000));
// Rust -> JS raw stream: onmessage gets an ArrayBuffer per send (verified, in order)
const ch = new Channel(); ch.onmessage = (buf) => { const dv = new DataView(buf); /* ... */ };
await invoke('start_stream', { size: 30000, count: 300, intervalUs: 33333, onPacket: ch });
// SharedBuffer
chrome.webview.addEventListener('sharedbufferreceived', (e) => { const ab = e.getBuffer(); const meta = e.additionalData;
  const vf = new VideoFrame(new Uint8Array(ab), { format: 'BGRA', codedWidth: 1920, codedHeight: 1080, timestamp: 0 }); /* encode */ vf.close(); chrome.webview.releaseBuffer(ab); });
// Opus encoder config accepted (all keys echoed back by isConfigSupported)
const opusCfg = { codec: 'opus', sampleRate: 48000, numberOfChannels: 1, bitrate: 24000, bitrateMode: 'variable',
  opus: { format: 'opus', frameDuration: 20000, complexity: 5, useinbandfec: true, usedtx: true, packetlossperc: 10, signal: 'voice', application: 'voip' } };
// H.264 realtime (1080p needs level >= 4.0)
const vCfg = { codec: 'avc1.42E028', width: 1920, height: 1080, bitrate: 4_000_000, framerate: 30, latencyMode: 'realtime', hardwareAcceleration: 'prefer-hardware', avc: { format: 'annexb' } };
```

Notes:
- To pre-grant without touching webview2-com, `Builder::on_permission_request` is enough. Use `WebviewWindowBuilder::on_permission_request` for one window only.
- Other useful raw interfaces in webview2-com-sys 0.39.1, all available via `with_webview`:
  - `ICoreWebView2_19::SetMemoryUsageTargetLevel`
  - `ICoreWebView2Controller::SetIsVisible` (avoid during calls)
  - `ICoreWebView2_8::SetIsMuted`
  - `ICoreWebView2Profile4::SetPermissionState` (pre-seed a permission per origin)
- Dev mode: add `server: { headers: { 'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp' } }` to vite.config.ts.