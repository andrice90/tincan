# webview-media

## Summary
Stack checked on 2026-11-22: tauri 2.12.7 (tauri-runtime-wry 2.12.5, wry 0.57.3, webview2-com 0.39.1, windows 0.62.2), @tauri-apps/cli 2.12.6, @tauri-apps/api 2.12.5, create-tauri-app 4.7.5 (template `react-ts`: React 19, Vite 8, TS 6). The machine has WebView2 Runtime 157.0.4341.60 (Chromium 157.0.8103.80).

I built a probe Tauri app in the scratch folder. It compiled with no warnings, and I ran it in release with `custom-protocol` on. Results from this one machine (RTX 5080 + UHD 770):

- **Page context:** origin http://tauri.localhost, isSecureContext=true. With COOP/COEP set in `app.security.headers`, crossOriginIsolated=true and SharedArrayBuffer works.
- **WebCodecs:** all four classes are present. Opus encode and decode work, and every `opus.*` key (frameDuration, complexity, useinbandfec, usedtx, packetlossperc, signal, application, format) is echoed back as recognised. At 1080p realtime, H.264 encodes in hardware and software, AV1 encodes in hardware and software, and VP8/VP9 encode in software only. `avc1.42E01F` (level 3.1) is rejected at 1080p; use `avc1.42E028` or `avc1.640028`.
- **Raw-media APIs:** MediaStreamTrackProcessor and MediaStreamTrackGenerator both work on the main thread. AudioWorklet runs at 48 kHz.
- **Background throttling:** with the window minimized or hidden for 6 minutes, visibilityState stayed `visible` and timers, rAF and Channel delivery were never throttled. wry 0.57.3 never calls `SetIsVisible(false)` on minimize. The `backgroundThrottling` setting does nothing on Windows.
- **IPC (paced):**
  - Channel Rust->JS: 200 B p50 0.42 ms, 30 KB p50 2.0 ms.
  - invoke raw JS->Rust: p50 0.7 to 1.0 ms.
  - localhost WebSocket round trip: 0.08 to 0.16 ms.
  - WebView2 SharedBuffer: an 8.3 MB 1080p BGRA frame reaches JS in about 2.7 ms.
- **Idle RAM:** about 154 MB private across the process tree for a blank page.

**Microphone/camera permission:** with no handler, WebView2 shows its own prompt. Tauri 2.12.0+ has `Builder::on_permission_request` (and per-webview variants) to auto-allow. The raw `with_webview` + `add_PermissionRequested` path also compiles. Neither handler was triggered at runtime, because getUserMedia was not called.

**Screen share:** getDisplayMedia has no permission kind in WebView2. It raises `ScreenCaptureStarting` (ICoreWebView2_27, stable since SDK 1.0.2903.40, present in webview2-com-sys 0.39.1). That event can only cancel or defer; the picker is always WebView2's own. Two open WebView2 bugs matter:
- #2184: the picker is clipped to the webview bounds.
- #5763: a deadlock when the picker closes, reported on Tauri 2.12.1, wry 0.57.0 and webview2-com 0.39.1, essentially this stack.

**Echo cancellation:** Chromium 157 enables kChromeWideEchoCancellation by default on Windows, which cancels all Chromium audio played to that output device. So a WebCodecs + AudioWorklet pipeline should keep echo cancellation, but this is not verified in WebView2.

## Recommendation
1. **Base stack:** Tauri 2.12.7 with WebView2. Do capture and codec work in the webview: getUserMedia/getDisplayMedia, then WebCodecs, then encoded frames to Rust/iroh.
   - Audio: Opus, mono, 48 kHz, 20 ms frames, 24-32 kbps VBR, `{useinbandfec:true, usedtx:true, packetlossperc:10, signal:'voice', application:'voip', complexity:5-8}`. Measured packets were about 66-93 bytes.
   - Video: H.264 `avc1.42E028` (1080p) or `avc1.42E01F` (720p) with `hardwareAcceleration:'prefer-hardware'`, `latencyMode:'realtime'`, `avc:{format:'annexb'}`. Fall back to the software encoder (also measured working), with AV1 as an option.
   - Pick codecs on each machine at runtime with `isConfigSupported`, because hardware encode support varies by GPU. Don't count on hardware H.264 having low tail latency: p99 was 355 ms in one paced run.
2. **Permissions:**
   - Auto-grant mic/camera with `tauri::Builder::on_permission_request(|_, k| match k { Microphone | Camera => Allow, _ => Default })`. No webview2-com code is needed for this.
   - Keep the small `with_webview` block only for `ICoreWebView2_27::add_ScreenCaptureStarting`, for logging or policy.
   - Optionally use `SetSavesInProfile(false)` so a decision isn't saved to the profile.
3. **Screen share:**
   - Ship getDisplayMedia first, since it is the least work. Keep the window large while the picker is open (bug #2184).
   - Budget for a native fallback because of the #5763 deadlock: Windows Graphics Capture in Rust (`windows-capture` 2.0.1, or the `windows` crate directly), then a WebView2 SharedBuffer ring (about 2.7 ms per 8 MB frame), then `new VideoFrame(u8, {format:'BGRA'})`, then the same WebCodecs encoder.
   - Don't use `windows-capture`'s own `VideoEncoder`: it writes MP4 through MediaTranscoder and is not a low-latency stream.
4. **IPC:**
   - Tauri IPC is enough for control traffic, audio (50 pps) and one or two video streams.
     - Rust->JS: `Channel<InvokeResponseBody>` with `InvokeResponseBody::Raw`.
     - JS->Rust: `invoke(cmd, Uint8Array, {headers})` with `tauri::ipc::Request` / `InvokeBody::Raw`.
   - For several inbound video streams, or to save CPU, move media to a localhost WebSocket (or WebSocketStream). Measured round trips were 5-10x lower. It must:
     - bind to 127.0.0.1 on a random port;
     - require a random token handed over via invoke;
     - reject connections whose Origin is not http://tauri.localhost (any browser tab can open ws://127.0.0.1).
   - Keep sync `#[tauri::command]` handlers tiny, because sync commands run on the IPC/main thread. Hand work to a tokio channel or use `async`.
   - Don't pace with `tokio::time::sleep` (20 ms became about 31 ms here). Pace from capture callbacks.
5. **Throttling:**
   - Nothing is needed on Windows with wry 0.57.3. Never call `controller.SetIsVisible(false)` while in a call.
   - Stop your own rAF loops and animations when minimized: rAF kept running at 240 Hz while minimized or hidden.
   - When idle in the tray and not in a call, you can lower memory with `ICoreWebView2_19::SetMemoryUsageTargetLevel(LOW)` via `with_webview`.
6. **COOP/COEP:**
   - Set them in `app.security.headers` for production. In dev, also set them in Vite `server.headers`, because desktop dev loads devUrl directly with no proxy.
   - Serve attachments and avatars same-origin (custom protocol or blob:). With COEP `require-corp`, cross-origin images without CORP/CORS will not load.
7. **Echo cancellation:** request `{audio:{echoCancellation:true, noiseSuppression:true, autoGainControl:true}}` and play remote audio through Web Audio / AudioWorklet. Chrome-wide AEC should cover that playback. Test early with two real machines. Try `echoCancellation:'all'` where it is accepted.
8. **Packaging:** NSIS with `bundle.windows.webviewInstallMode: {type:'downloadBootstrapper', silent:true}` (the default), or `embedBootstrapper` (+1.8 MB) for offline-safe installs, and `nsis.installMode: 'currentUser'` (the default, no admin prompt).

## Versions
- tauri (crate) 2.12.7 edition 2024, MSRV 1.90; on_permission_request since 2.12.0
- tauri-runtime-wry 2.12.5 
- tauri-runtime 2.12.4 
- tauri-utils 2.10.4 
- tauri-build 2.7.6 
- tauri-macros 2.7.6 
- wry 0.57.3 never calls SetIsVisible(false) on minimize; backgroundThrottling unsupported on Windows
- tao 0.37.3 
- webview2-com / webview2-com-sys 0.39.1 must match tauri's ^0.39; includes ICoreWebView2_27 ScreenCaptureStarting, _17 SharedBuffer
- windows / windows-core 0.62.2 
- @tauri-apps/cli 2.12.6 
- @tauri-apps/api 2.12.5 
- create-tauri-app 4.7.5 --template react-ts: react ^19.1.0, vite ^8.0.16, typescript ~6.0.3
- windows-capture 2.0.1 WGC/DXGI capture; its VideoEncoder is container-based (MediaTranscoder)
- tokio-tungstenite 0.30.0 used for the localhost WS comparison
- WebView2 Runtime (dev machine) 157.0.4341.60 Chromium 157.0.8103.80
- WebView2 SDK: ScreenCaptureStarting (ICoreWebView2_27) 1.0.2903.40 first stable SDK
- Rust toolchain 1.99.1 

## Risks
- getDisplayMedia on this exact stack (Tauri 2.12.x / wry 0.57 / webview2-com 0.39.1) has an open, tracked WebView2 deadlock when the window picker closes (#5763). The picker is also clipped to the webview bounds (#2184) and lists the host app's own windows. Screen share may need the native fallback: WGC capture, SharedBuffer, WebCodecs.
- The picker cannot be skipped or pre-selected. ScreenCaptureStarting can only cancel or defer it, so the screen-share UX stays WebView2's dialog.
- Hardware encoder availability depends on the machine. This RTX 5080 box had hardware H.264 and AV1 but no hardware VP8/VP9 encode. Hardware H.264 tail latency was erratic (p99 355 ms in one paced run). Weaker GPUs or iGPU-only laptops may only get software encoders, which cost more CPU. Negotiate codecs per peer.
- Echo cancellation for audio played through Web Audio/AudioWorklet relies on Chromium's chrome-wide AEC. It is on by default in Chromium 157 source, but Edge/WebView2 can override feature defaults through ECS/Finch, which I could not verify. If AEC does not cover AudioWorklet playback, users on speakers will hear echo. The fallbacks are playing through an RTCPeerConnection loopback, or using echoCancellation:'all'.
- No background throttling on Windows also means no automatic savings: rAF ran at 240 Hz while minimized or hidden. The app must pause its own rendering. If a future wry starts calling SetIsVisible(false) on minimize, timers and media could suddenly be throttled; re-test after each wry upgrade.
- COEP require-corp (needed for SharedArrayBuffer) blocks cross-origin images and iframes that lack CORP/CORS headers, for example link previews or remote avatars. Serve such content through the app's own protocol, or skip crossOriginIsolated if SAB is not needed.
- app.security.headers only applies to the built app. In desktop dev the devUrl is loaded directly, so crossOriginIsolated will be false in dev unless Vite sets the headers.
- Every Channel message of 1024 bytes or more costs an eval plus a fetch round trip, and a burst queues: 30 KB x 300 gave about 80 ms of queueing latency. With many inbound video streams, CPU and latency could grow. I did not measure CPU per message.
- Sync #[tauri::command] handlers run on the WebView2 UI thread. Slow work in a per-packet handler will stall the UI and all IPC.
- A localhost WebSocket media path can be reached by any local process and by any web page in the user's browser. It needs a per-session token and an Origin check.
- Microphone/camera permission decisions persist in the WebView2 profile by default. A user's earlier 'Block' from the built-in prompt would stop the Tauri handler from being consulted.
- tokio::time::sleep pacing on Windows is coarse (20 ms became about 31 ms). Do not use timers to pace media. Driving output from capture callbacks avoids it.

## Open
- Manual click-test needed: call getUserMedia({audio:true}) and confirm Builder::on_permission_request auto-allows it with no prompt. Then call getDisplayMedia({video:true, audio:true, systemAudio:'include'}) and record: the picker (screen and window tabs, 'share system audio' checkbox), whether ScreenCaptureStarting fires, whether a system-audio track comes back, and whether the #5763 freeze happens on WebView2 157.
- Two-machine AEC loopback test: machine A plays remote Opus through AudioWorklet to speakers, and machine B checks whether A's mic sends B's voice back. Compare echoCancellation:true with 'all', and with <audio> playback, to confirm chrome-wide AEC in WebView2.
- Is there throttling when the Tauri window is fully covered by a fullscreen game (occlusion) rather than minimized? Minimize and hide showed none, which suggests WebView2 does not track host occlusion, but this is untested.
- AudioWorklet and WebCodecs were only observed for 15 s while minimized. A long call minimized under real load (encoders running) should be soak-tested.
- CPU cost of Channel/invoke versus localhost WebSocket versus SharedBuffer at realistic loads (for example 4 inbound 30 fps streams plus 8 audio streams) was not measured; it matters for the minimum-resources goal.
- Does controller.SetIsVisible(false) (minimize-to-tray done properly) suspend timers, the worklet or WebCodecs? It is worth knowing for an idle tray mode outside calls.
- How does hardware encode behave on low-end or iGPU-only machines and with the NVIDIA encoder session limit? Real-world fallback rates are unknown.
- Low-latency H.264 encode in Rust from WGC frames (Media Foundation MFT via the windows crate, or openh264) was not researched. That would avoid the webview round trip in the native screen-share fallback.
