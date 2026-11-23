# tincan-screen: integration notes

Crate: `crates/tincan-screen` (Windows only). WGC capture through `moq_video::capture`, GPU downscale, Media Foundation hardware H.264. Each share runs on its own `tincan-screen` thread with a current-thread tokio runtime that owns capture and encoder. Rustdoc: `cargo doc -p tincan-screen --no-deps`. Example: `cargo run -p tincan-screen --example capture_to_file -- [source-id|list] [seconds] [out.h264] [text|motion]`.

## API

- `sources() -> Result<Vec<ScreenSource>>` (async). Displays first (primary first), then visible top-level windows in z-order. Skips cloaked, untitled and tool windows, windows under 64 px, "Program Manager", windows of the app `tincan` and of this process.
  - `ScreenSource { id, kind, name, width, height }`. `id` is `display:<index>` (0 = primary) or `window:<HWND>`. Sizes are physical pixels. `kind.as_str()` is `"display" | "window"` (types.ts).
  - Must run inside a Tokio runtime (moq calls `spawn_blocking`).
  - Display ids are moq indices: they can point at another monitor if monitors change between `sources()` and `start()`.
- `hardware_encoder_available() -> bool`. The first call blocks about 240 ms (opens one MFT on a fresh thread), so call it via `spawn_blocking`; the answer is cached for the process.
- `ShareMode { Text (default), Motion }`: `max_fps` 15 / 30, start bitrate 1.2 / 3 Mbps, cap 2.5 / 4.5 Mbps, `parse("text"|"motion")`, `as_str()`.
- `ScreenShare::start(source_id, mode, frames: mpsc::Sender<VideoFrame>) -> Result<ScreenShare>` (any executor). Returns after capture is open and the first frame is encoded (about 130 ms warm, 280 ms on the first share).
  - Errors: `NoHardwareEncoder` (`code() == Some("no-hardware-encoder")`, the api.md machine string), `InvalidSource`, `SourceUnavailable` (window gone or minimized, display missing), `PermissionDenied`, `Unsupported`, `Capture`, `Encoder`, `Thread`.
  - Frames: one Annex-B access unit per `VideoFrame`. Keyframes are AUD, SPS, PPS, IDR (the AUD comes first: find the SPS by NAL scan, as `tincan_core::media::h264_codec_string` does). The first frame is a keyframe. `ts` is 90 kHz, strictly increasing within a share, wraps at 2^32. A new share starts a new timeline.
  - Sent with `try_send`: when the channel is full the frame and the following deltas are dropped and a keyframe is forced. A closed channel ends the share with `ReceiverClosed`.
- `request_keyframe()`: at most one forced keyframe per 500 ms; a request inside the window fires when it ends.
- `set_bitrate(bps)`: clamped to 300 kbps..=mode cap. Below 600 kbps the share drops to a 720p-class 10 fps tier (encoder rebuilt, one keyframe) and returns at 750 kbps or more.
- `stats() -> ShareStats { fps, bitrate_bps (measured over 2 s), target_bitrate_bps, width, height, frames_sent, frames_dropped, keyframes_sent }`.
- `ended() -> watch::Receiver<Option<EndReason>>`: `None` while running, then `Some(Stopped | SourceClosed(_) | ReceiverClosed | Failed(_))` once. Watch it to tell the UI the share ended (window closed).
- `stop(self)` (and `Drop`) joins the thread and blocks about 30-50 ms: call it from `spawn_blocking` or another blocking-tolerant context, not on a hot async worker.

## Process-wide side effects

- The first share calls `CoIncrementMTAUsage` and never releases it: the COM MTA exists for the rest of the process. Threads that never initialise COM count as implicit MTA members afterwards. Anything else in the process that needs an STA must initialise its own thread as STA explicitly.
- After a share, one D3D11 device (tens of MB) stays alive in moq's scaler cache until the next share or exit.
- About 8 handles leak inside Media Foundation per share; threads and memory stay flat.

## Not verified

Display unplugged mid-share, machines without a hardware encoder (only the error mapping is tested), AMD or Intel-only GPUs, VMs, hybrid laptops where the first MFT is on another adapter than the capture device, WGC `Closed` for windows of other processes (the share polls `IsWindow` instead), release builds.
