//! One screen share: capture, scale and hardware-encode on a dedicated thread.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use moq_video::capture;
use moq_video::encode::{Config as EncoderConfig, Encoded, Encoder, Gop, Kind};
use moq_video::windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use moq_video::windows::Win32::System::Com::CoIncrementMTAUsage;
use moq_video::{Color, Frame, Rate, Size, Surface};
use parking_lot::Mutex;
use tincan_core::media::{VideoFrame, h264_is_keyframe};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::{Notify, oneshot, watch};

use crate::policy::{
    GOP, KEYFRAME_MIN_INTERVAL, KeyframeGate, Limits, Meter, RateLimiter, ShareMode, Tier, Timeline, to_90khz,
};
use crate::source::{SourceKind, parse_id, window_exists};
use crate::{Error, Result, hardware_encoder_available};

/// How many times a running share rebuilds a failing encoder before giving up.
const MAX_ENCODER_REBUILDS: u32 = 3;

/// Capture is paced at this multiple of the mode's frame cap (see `session`).
const CAPTURE_OVERSAMPLE: u32 = 2;

/// How often a window share checks that its window still exists while no
/// frames arrive (a minimized window delivers none).
const WINDOW_CHECK: Duration = Duration::from_secs(1);

/// Window the [`ShareStats`] rates are averaged over.
const STATS_WINDOW: Duration = Duration::from_secs(2);

/// A snapshot of a running share, from [`ScreenShare::stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShareStats {
    /// Frames per second actually sent over the last two seconds.
    pub fps: f32,
    /// Bitrate actually sent over the last two seconds (measured from access
    /// unit sizes, not the encoder target), in bits per second.
    pub bitrate_bps: u32,
    /// The encoder's current target bitrate (after clamping), bits per second.
    pub target_bitrate_bps: u32,
    /// Encoded width in pixels (after any downscale).
    pub width: u32,
    /// Encoded height in pixels (after any downscale).
    pub height: u32,
    /// Access units delivered to the frame channel since the start.
    pub frames_sent: u64,
    /// Access units discarded since the start: the frame channel was full, or
    /// they were deltas waiting for the keyframe that follows a drop.
    pub frames_dropped: u64,
    /// Keyframes delivered since the start.
    pub keyframes_sent: u64,
}

/// Why a share ended, as reported by [`ScreenShare::ended`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// [`ScreenShare::stop`] was called or the share was dropped.
    Stopped,
    /// The shared window closed or the display went away.
    SourceClosed(String),
    /// The receiving end of the frame channel was dropped.
    ReceiverClosed,
    /// Capture or encoding failed and could not recover.
    Failed(String),
}

/// A running screen share.
///
/// Capture (Windows.Graphics.Capture), GPU downscale and Media Foundation
/// hardware H.264 encoding run on a dedicated `tincan-screen` thread (plus the
/// capture's own `moq-wgc` thread). Encoded access units go to the frame
/// channel given to [`ScreenShare::start`]; nothing is ever queued inside this
/// crate. Dropping the share stops it like [`ScreenShare::stop`].
pub struct ScreenShare {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    ended: watch::Receiver<Option<EndReason>>,
    kind: SourceKind,
    source_id: String,
}

/// State shared between the handle and the share thread.
struct Shared {
    mode: ShareMode,
    stop: AtomicBool,
    wake: Notify,
    keyframe: AtomicBool,
    bitrate: AtomicU32,
    width: AtomicU32,
    height: AtomicU32,
    sent: AtomicU64,
    dropped: AtomicU64,
    keyframes: AtomicU64,
    meter: Mutex<Meter>,
}

impl Shared {
    fn new(mode: ShareMode) -> Self {
        Self {
            mode,
            stop: AtomicBool::new(false),
            wake: Notify::new(),
            keyframe: AtomicBool::new(false),
            bitrate: AtomicU32::new(mode.target_bitrate_bps()),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            keyframes: AtomicU64::new(0),
            meter: Mutex::new(Meter::new(STATS_WINDOW, Instant::now())),
        }
    }
}

impl ScreenShare {
    /// Start sharing `source_id` (from [`sources`](crate::sources)) in `mode`.
    ///
    /// Encoded frames go to `frames` with `try_send`: each [`VideoFrame`] is one
    /// Annex-B access unit, keyframes carry SPS/PPS in band (an access unit
    /// delimiter NAL may precede them), `ts` is 90 kHz from capture time on a
    /// monotonic timeline, and `keyframe` comes from
    /// [`h264_is_keyframe`]. The first frame sent is always a keyframe. When the
    /// channel is full the frame is dropped (never queued), the deltas after it
    /// are dropped too, and a keyframe is forced (rate limited to one per
    /// 500 ms) so the receiver can resume cleanly.
    ///
    /// Returns once capture is open and the first frame has gone through the
    /// encoder, so a missing encoder or a bad source is reported here rather
    /// than through [`ScreenShare::ended`]. Typically 100-300 ms. Does not need
    /// a particular async runtime.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidSource`] for an id `sources` would not produce.
    /// - [`Error::NoHardwareEncoder`] when this machine cannot encode (see
    ///   [`hardware_encoder_available`]).
    /// - [`Error::SourceUnavailable`] when the window is gone or minimized, or
    ///   the display index does not exist.
    /// - [`Error::PermissionDenied`], [`Error::Unsupported`], [`Error::Capture`],
    ///   [`Error::Encoder`], [`Error::Thread`] for the rarer failures.
    pub async fn start(source_id: &str, mode: ShareMode, frames: mpsc::Sender<VideoFrame>) -> Result<ScreenShare> {
        let (kind, source) = parse_id(source_id)?;
        let shared = Arc::new(Shared::new(mode));
        let (end_tx, end_rx) = watch::channel(None);
        let (ready_tx, ready_rx) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("tincan-screen".into())
            .spawn({
                let shared = shared.clone();
                let target = Target {
                    id: source_id.to_owned(),
                    kind,
                    source,
                };
                move || thread_main(target, frames, shared, end_tx, ready_tx)
            })
            .map_err(|err| Error::Thread(format!("spawn: {err}")))?;
        match ready_rx.await {
            Ok(Ok(())) => Ok(ScreenShare {
                shared,
                thread: Some(thread),
                ended: end_rx,
                kind,
                source_id: source_id.to_owned(),
            }),
            // The thread reports a start failure only after releasing capture
            // and the encoder, and exits right after; no need to wait for it.
            Ok(Err(err)) => Err(err),
            Err(_) => Err(Error::Thread("the share thread exited during start".into())),
        }
    }

    /// Ask for a keyframe (a viewer joined or lost data). Applied to the next
    /// captured frame; requests are coalesced to at most one forced keyframe
    /// per 500 ms, and a request inside that window fires when it expires.
    pub fn request_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Release);
    }

    /// Set the target bitrate in bits per second, clamped to 300 kbps ..= the
    /// mode cap (2.5 Mbps text, 4.5 Mbps motion). Applied from the next frame
    /// without a keyframe. Below 600 kbps the share switches to a 720p-class,
    /// 10 fps tier (one keyframe at the switch) and returns at 750 kbps.
    pub fn set_bitrate(&self, bps: u32) {
        let clamped = self.shared.mode.clamp_bitrate(bps);
        self.shared.bitrate.store(clamped, Ordering::Release);
    }

    /// Current rates and counters.
    pub fn stats(&self) -> ShareStats {
        let s = &self.shared;
        let (fps, bitrate_bps) = s.meter.lock().rates(Instant::now());
        ShareStats {
            fps,
            bitrate_bps,
            target_bitrate_bps: s.bitrate.load(Ordering::Acquire),
            width: s.width.load(Ordering::Acquire),
            height: s.height.load(Ordering::Acquire),
            frames_sent: s.sent.load(Ordering::Acquire),
            frames_dropped: s.dropped.load(Ordering::Acquire),
            keyframes_sent: s.keyframes.load(Ordering::Acquire),
        }
    }

    /// Completion signal: `None` while the share runs, then `Some(reason)`
    /// exactly once when it ends for any reason (window closed, display
    /// removed, receiver dropped, failure, or stop). Await
    /// `receiver.wait_for(Option::is_some)` to be told.
    pub fn ended(&self) -> watch::Receiver<Option<EndReason>> {
        self.ended.clone()
    }

    /// Why the share ended, or `None` while it is still running.
    pub fn end_reason(&self) -> Option<EndReason> {
        self.ended.borrow().clone()
    }

    /// The mode this share was started in.
    pub fn mode(&self) -> ShareMode {
        self.shared.mode
    }

    /// The source id this share was started with.
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// Whether the source is a display or a window.
    pub fn source_kind(&self) -> SourceKind {
        self.kind
    }

    /// Stop capturing and encoding and join the share thread. Prompt: the
    /// thread is woken directly, capture is released, moq-video's GPU scaler
    /// cache is flushed of the session's device and the encoder is drained
    /// and released. Blocks the caller for that, measured at about 50 ms
    /// (the cache flush is most of it). Dropping the share does the same.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("screen share thread panicked");
        }
    }
}

impl Drop for ScreenShare {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for ScreenShare {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreenShare")
            .field("source_id", &self.source_id)
            .field("mode", &self.shared.mode)
            .field("ended", &*self.ended.borrow())
            .finish_non_exhaustive()
    }
}

/// The start handshake: fires once, after the first frame was encoded.
struct Ready(Option<oneshot::Sender<Result<()>>>);

impl Ready {
    fn is_pending(&self) -> bool {
        self.0.is_some()
    }

    /// Report a successful start. False if `start()` was cancelled.
    fn fire(&mut self) -> bool {
        self.0.take().is_none_or(|tx| tx.send(Ok(())).is_ok())
    }

    /// Report a failed start (no-op once started).
    fn fail(&mut self, err: Error) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(Err(err));
        }
    }
}

/// Publishes the end reason on every exit path, panics included.
struct EndGuard {
    tx: watch::Sender<Option<EndReason>>,
    reason: Option<EndReason>,
}

impl Drop for EndGuard {
    fn drop(&mut self) {
        let reason = self
            .reason
            .take()
            .unwrap_or_else(|| EndReason::Failed("screen share thread exited unexpectedly".into()));
        self.tx.send_replace(Some(reason));
    }
}

/// What a share captures.
struct Target {
    id: String,
    kind: SourceKind,
    source: capture::Source,
}

fn thread_main(
    target: Target,
    frames: mpsc::Sender<VideoFrame>,
    shared: Arc<Shared>,
    end_tx: watch::Sender<Option<EndReason>>,
    ready_tx: oneshot::Sender<Result<()>>,
) {
    let mut end = EndGuard {
        tx: end_tx,
        reason: None,
    };
    let mut ready = Ready(Some(ready_tx));
    keep_mta_alive();
    if !hardware_encoder_available() {
        ready.fail(Error::NoHardwareEncoder);
        end.reason = Some(EndReason::Failed(Error::NoHardwareEncoder.to_string()));
        return;
    }
    // A runtime of our own: capture delivers frames through async channels,
    // and the encoder is bound to this thread, so the loop never leaves it.
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_time().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            let err = Error::Thread(format!("runtime: {err}"));
            end.reason = Some(EndReason::Failed(err.to_string()));
            ready.fail(err);
            return;
        }
    };
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(session(target, frames, &shared, &mut ready))
    }));
    drop(runtime);
    let reason = match outcome {
        Ok(Ok(reason)) => reason,
        Ok(Err(err)) => {
            let reason = EndReason::Failed(err.to_string());
            ready.fail(err);
            reason
        }
        Err(_) => {
            let err = Error::Thread("the share thread panicked".into());
            let reason = EndReason::Failed(err.to_string());
            ready.fail(err);
            reason
        }
    };
    if ready.is_pending() {
        // Ended cleanly but before the first frame was encoded.
        ready.fail(match &reason {
            EndReason::SourceClosed(m) => Error::SourceUnavailable(m.clone()),
            EndReason::ReceiverClosed => Error::Thread("the frame receiver was dropped during start".into()),
            EndReason::Failed(m) => Error::Thread(m.clone()),
            EndReason::Stopped => Error::Thread("stopped during start".into()),
        });
    }
    tracing::info!(?reason, "screen share ended");
    end.reason = Some(reason);
}

/// Hold one reference on the process's multithreaded COM apartment for the
/// life of the process.
///
/// windows-rs caches WinRT activation factories in process-wide statics, while
/// moq-video's capture thread enters and leaves the MTA once per session. When
/// no other thread was in the MTA as a session ended, the apartment was torn
/// down and the cached factories dangled: the next share crashed with an access
/// violation inside `GraphicsCaptureSession::IsSupported` (reproduced on the
/// dev machine: share, idle, share). `CoIncrementMTAUsage` keeps the MTA, and
/// so the cache, valid without parking a thread in it.
fn keep_mta_alive() {
    static HELD: OnceLock<bool> = OnceLock::new();
    HELD.get_or_init(|| {
        // SAFETY: CoIncrementMTAUsage takes no pointers from us and may be
        // called from any thread, COM-initialized or not. The cookie is never
        // passed to CoDecrementMTAUsage on purpose.
        match unsafe { CoIncrementMTAUsage() } {
            Ok(_cookie) => true,
            Err(err) => {
                tracing::warn!(%err, "could not pin the COM MTA; repeated shares may be unstable");
                false
            }
        }
    });
}

/// Resolves once `stop` is set. Cancel-safe: the flag is re-checked after
/// every wakeup and `notify_one` keeps a permit if nobody is waiting yet.
async fn stopped(shared: &Shared) {
    loop {
        let notified = shared.wake.notified();
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

/// Map a capture error to the start error (before the first frame) or the
/// end reason (after).
fn capture_ended(err: moq_video::Error, ready: &Ready) -> Result<EndReason> {
    let err = Error::from_capture(err);
    if ready.is_pending() {
        return Err(err);
    }
    Ok(match err {
        Error::SourceUnavailable(m) => EndReason::SourceClosed(m),
        other => EndReason::Failed(other.to_string()),
    })
}

/// The shared window is gone: a start error before the first frame, an end
/// reason after.
fn window_closed(ready: &Ready) -> Result<EndReason> {
    let message = "the shared window was closed".to_owned();
    if ready.is_pending() {
        Err(Error::SourceUnavailable(message))
    } else {
        Ok(EndReason::SourceClosed(message))
    }
}

/// What woke the share loop.
enum Wake {
    Stop,
    CheckWindow,
    Read(std::result::Result<Option<Frame>, moq_video::Error>),
}

/// The share loop. Returns `Err` only for failures before the first encoded
/// frame (they become `start()` errors); later failures are end reasons.
async fn session(
    target: Target,
    frames: mpsc::Sender<VideoFrame>,
    shared: &Shared,
    ready: &mut Ready,
) -> Result<EndReason> {
    let mut pipeline = Pipeline::new(shared.mode, shared.bitrate.load(Ordering::Acquire));
    let result = capture_loop(target, &frames, shared, ready, &mut pipeline).await;
    // Capture is released by now; let go of its GPU device before the encoder.
    pipeline.release_gpu_caches();
    result
}

async fn capture_loop(
    target: Target,
    frames: &mpsc::Sender<VideoFrame>,
    shared: &Shared,
    ready: &mut Ready,
    pipeline: &mut Pipeline,
) -> Result<EndReason> {
    let mode = shared.mode;
    let watch_window = target.kind == SourceKind::Window;
    let mut window_check = tokio::time::interval(WINDOW_CHECK);
    window_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut config = capture::Config::default();
    config.source = target.source;
    // moq-video paces WGC delivery at this rate and repeats the last picture
    // while the screen is static, so frames keep flowing for keyframe requests.
    // Its pacing sleeps on a condvar, which Windows rounds up to the 15.6 ms
    // timer tick (66.7 ms becomes 78 ms, 13 fps instead of 15; 33.3 ms becomes
    // 47 ms, 21 fps instead of 30). Asking for twice the cap and letting the
    // rate limiter keep the exact cap gives the full frame rate.
    config.framerate =
        Some(Rate::new(CAPTURE_OVERSAMPLE * mode.max_fps(), 1).map_err(|e| Error::Capture(e.to_string()))?);
    config.cursor = true;

    let mut timeline = Timeline::new(Instant::now());

    // One iteration per capture stream: a resized source ends its stream with
    // `Ok(None)` and is reopened at the new size.
    loop {
        let opened = Instant::now();
        let open = tokio::select! {
            biased;
            () = stopped(shared) => return Ok(EndReason::Stopped),
            open = capture::open(&config) => open,
        };
        let mut stream = match open {
            Ok(stream) => stream,
            Err(err) => return capture_ended(err, ready),
        };
        timeline.rebase(opened);
        tracing::info!(
            source = stream.label(),
            width = stream.width(),
            height = stream.height(),
            "screen capture open"
        );

        loop {
            let wake = tokio::select! {
                biased;
                () = stopped(shared) => Wake::Stop,
                _ = window_check.tick(), if watch_window => Wake::CheckWindow,
                read = stream.read() => Wake::Read(read),
            };
            let read = match wake {
                Wake::Stop => return Ok(EndReason::Stopped),
                Wake::CheckWindow if !window_exists(&target.id) => return window_closed(ready),
                Wake::CheckWindow => continue,
                Wake::Read(read) => read,
            };
            match read {
                Ok(Some(_)) if watch_window && !window_exists(&target.id) => return window_closed(ready),
                Ok(Some(frame)) => {
                    match pipeline.push(frame, &mut timeline, frames, shared) {
                        Ok(Flow::Continue) => {}
                        Ok(Flow::ReceiverClosed) => return Ok(EndReason::ReceiverClosed),
                        Err(err) if ready.is_pending() => return Err(err),
                        Err(err) => return Ok(EndReason::Failed(err.to_string())),
                    }
                    if pipeline.encoded_any() && ready.is_pending() && !ready.fire() {
                        tracing::debug!("start() was cancelled; ending the share");
                        return Ok(EndReason::Stopped);
                    }
                }
                Ok(None) => {
                    tracing::info!("screen source changed size; reopening capture");
                    // Drop the stream (and its device) before flushing caches.
                    drop(stream);
                    pipeline.capture_closed();
                    break;
                }
                Err(err) => return capture_ended(err, ready),
            }
        }
    }
}

/// What the loop does after a frame.
enum Flow {
    Continue,
    ReceiverClosed,
}

/// Rate limit, scale, keyframe policy and encode for one share. Lives on the
/// share thread for its whole life (the encoder is thread-bound).
struct Pipeline {
    mode: ShareMode,
    tier: Tier,
    limits: Limits,
    limiter: RateLimiter,
    gate: KeyframeGate,
    /// Target bitrate the loop has accepted from the handle.
    bitrate: u32,
    active: Option<ActiveEncoder>,
    /// Discard deltas until a keyframe is sent (start, rebuild, after a drop).
    waiting_for_key: bool,
    /// When the last keyframe was produced or forced, for the wall-clock GOP.
    last_keyframe: Option<Instant>,
    /// At least one frame went through an encoder.
    encoded: bool,
    /// Encoder failures since the last good encode.
    failures: u32,
    /// The last downscaled picture, reused while capture repeats it.
    scaled: Option<Scaled>,
    /// The last captured picture, kept to flush moq-video's scaler cache.
    last_picture: Option<Frame>,
}

/// Capacity of moq-video's process-wide video processor cache (its
/// `SCALER_CACHE_CAPACITY`, private in moq-video 0.1.23).
const MOQ_SCALER_CACHE_CAPACITY: u32 = 16;

/// A downscaled picture and the capture texture it was made from.
///
/// While the screen is static, capture re-delivers the same texture at its
/// pacing rate. Scaling it again each time would allocate a texture and two
/// views per frame for an identical result (measured: 5% of a core on a
/// 3440x1440 display versus 0.3% unscaled), so the scaled frame is re-encoded
/// instead. Holding `source` keeps its pointer from being reused by another
/// texture while it is compared.
struct Scaled {
    source: ID3D11Texture2D,
    target: Size,
    frame: Frame,
}

/// The GPU texture behind a captured frame, if it is one.
fn source_texture(frame: &Frame) -> Option<ID3D11Texture2D> {
    match &frame.surface {
        Surface::Texture(texture) => Some(texture.texture().clone()),
        _ => None,
    }
}

impl Pipeline {
    fn new(mode: ShareMode, bitrate: u32) -> Self {
        let tier = Tier::Normal.next(bitrate);
        let limits = Limits::of(mode, tier);
        Self {
            mode,
            tier,
            limits,
            limiter: RateLimiter::new(limits.fps),
            gate: KeyframeGate::new(KEYFRAME_MIN_INTERVAL),
            bitrate,
            active: None,
            waiting_for_key: true,
            last_keyframe: None,
            encoded: false,
            failures: 0,
            scaled: None,
            last_picture: None,
        }
    }

    /// The capture stream ended: drop what refers to its device. Every capture
    /// session creates its own D3D11 device and the encoder is bound to the
    /// device of the first texture it saw, so the next stream gets a new
    /// encoder (and starts with a keyframe) even if its size is unchanged.
    fn capture_closed(&mut self) {
        self.release_gpu_caches();
        self.active = None;
    }

    /// Let go of the capture session's D3D11 device.
    ///
    /// moq-video keeps one D3D11 video processor per (device, scale) in a
    /// process-wide LRU of 16 and evicts only beyond 16, so every capture
    /// session's device (a full D3D11 device with its driver threads) stayed
    /// alive until 16 newer entries pushed it out: measured +145 threads,
    /// +1000 handles and +400 MB after 20 shares. Filling the cache with
    /// entries for the current device (small scales of its last picture)
    /// evicts every older idle entry, so at most one device outlives a share.
    fn release_gpu_caches(&mut self) {
        self.scaled = None;
        let Some(picture) = self.last_picture.take() else {
            return;
        };
        if !matches!(picture.surface, Surface::Texture(_)) {
            return;
        }
        let started = Instant::now();
        for i in 0..MOQ_SCALER_CACHE_CAPACITY {
            if let Err(err) = picture.resize(Size::new(64 + 2 * i, 64), &moq_video::resize::Config::default()) {
                tracing::debug!(%err, "stopped flushing the scaler cache");
                break;
            }
        }
        tracing::debug!(elapsed = ?started.elapsed(), "flushed the GPU scaler cache");
    }

    fn encoded_any(&self) -> bool {
        self.encoded
    }

    /// Pick up keyframe requests and bitrate changes from the handle.
    fn apply_controls(&mut self, shared: &Shared) {
        if shared.keyframe.swap(false, Ordering::AcqRel) {
            self.gate.request();
        }
        let bitrate = shared.bitrate.load(Ordering::Acquire);
        if bitrate == self.bitrate {
            return;
        }
        self.bitrate = bitrate;
        let tier = self.tier.next(bitrate);
        if tier != self.tier {
            tracing::info!(?tier, bitrate, "screen share quality tier changed");
            self.tier = tier;
            self.limits = Limits::of(self.mode, tier);
            self.limiter.set_fps(self.limits.fps);
        }
    }

    fn push(
        &mut self,
        mut frame: Frame,
        timeline: &mut Timeline,
        frames: &mpsc::Sender<VideoFrame>,
        shared: &Shared,
    ) -> Result<Flow> {
        let now = Instant::now();
        let t_us = timeline.map(u64::try_from(frame.timestamp.as_micros()).unwrap_or(u64::MAX));
        self.apply_controls(shared);
        if !self.limiter.admit(t_us) {
            return Ok(Flow::Continue);
        }

        let source = frame.size();
        let (width, height) = self.limits.fit(source.width, source.height);
        let target = Size::new(width, height);
        let color = frame.surface.color();
        let reusable = self
            .active
            .as_ref()
            .is_some_and(|a| a.size == target && a.fps == self.limits.fps && a.color == color);
        if !reusable {
            // Release the old session first: one hardware encoder at a time.
            self.active = None;
            match ActiveEncoder::open(target, self.limits.fps, self.bitrate, color) {
                Ok(active) => self.active = Some(active),
                Err(err) => return self.failed(err),
            }
            // A fresh encoder opens with an IDR; nothing before it is decodable.
            self.waiting_for_key = true;
            self.last_keyframe = Some(now);
            shared.width.store(width, Ordering::Release);
            shared.height.store(height, Ordering::Release);
        }
        let Some(active) = self.active.as_mut() else {
            return Ok(Flow::Continue);
        };
        active.retune(self.bitrate);

        let timestamp = moq_net::Timestamp::from_micros(t_us).map_err(|e| Error::Encoder(e.to_string()))?;
        let mut uncached = None;
        let input: &mut Frame = if source == target {
            self.scaled = None;
            &mut frame
        } else {
            let picture = source_texture(&frame);
            let hit = matches!((&self.scaled, &picture), (Some(s), Some(p)) if s.target == target && s.source == *p);
            if !hit {
                // GPU video processor on the capture device (CPU fallback inside).
                let resized = match frame.resize(target, &moq_video::resize::Config::default()) {
                    Ok(resized) => resized,
                    Err(err) => return self.failed(Error::from_encoder(err)),
                };
                match picture {
                    Some(source) => {
                        self.scaled = Some(Scaled {
                            source,
                            target,
                            frame: resized,
                        })
                    }
                    None => {
                        self.scaled = None;
                        uncached = Some(resized);
                    }
                }
            }
            match (uncached.as_mut(), self.scaled.as_mut()) {
                (Some(resized), _) => resized,
                (None, Some(scaled)) => &mut scaled.frame,
                (None, None) => &mut frame,
            }
        };
        input.timestamp = timestamp;

        let gop_due = self
            .last_keyframe
            .is_some_and(|last| now.saturating_duration_since(last) >= GOP);
        let requested = self.gate.poll(now);
        if (gop_due || requested) && active.cut() {
            self.last_keyframe = Some(now);
        }

        let units = match active.encoder.encode(input) {
            Ok(units) => units,
            Err(err) => {
                // A failed MFT may never finish a drain; do not wait on it.
                active.drain_on_drop = false;
                return self.failed(Error::from_encoder(err));
            }
        };
        self.encoded = true;
        if !units.is_empty() {
            self.failures = 0;
        }
        self.last_picture = Some(frame);
        for unit in units {
            if let Flow::ReceiverClosed = self.emit(unit, now, frames, shared) {
                return Ok(Flow::ReceiverClosed);
            }
        }
        Ok(Flow::Continue)
    }

    /// An encoder open, scale or encode failed. Before the first frame that is
    /// a start error; afterwards the encoder is rebuilt on the next frame, up
    /// to [`MAX_ENCODER_REBUILDS`] times in a row.
    fn failed(&mut self, err: Error) -> Result<Flow> {
        self.active = None;
        if !self.encoded {
            return Err(err);
        }
        self.failures += 1;
        if self.failures > MAX_ENCODER_REBUILDS {
            return Err(err);
        }
        tracing::warn!(%err, attempt = self.failures, "screen encoder failed; rebuilding it");
        Ok(Flow::Continue)
    }

    /// Hand one access unit to the frame channel, applying the drop policy.
    fn emit(&mut self, unit: Encoded, now: Instant, frames: &mpsc::Sender<VideoFrame>, shared: &Shared) -> Flow {
        let timestamp = unit.timestamp;
        let data = unit.payload;
        let keyframe = h264_is_keyframe(&data);
        if keyframe {
            self.gate.on_keyframe(now);
            self.last_keyframe = Some(now);
        }
        if self.waiting_for_key && !keyframe {
            shared.dropped.fetch_add(1, Ordering::AcqRel);
            return Flow::Continue;
        }
        let ts = to_90khz(u64::try_from(timestamp.as_micros()).unwrap_or(u64::MAX));
        let len = data.len();
        match frames.try_send(VideoFrame { ts, keyframe, data }) {
            Ok(()) => {
                shared.sent.fetch_add(1, Ordering::AcqRel);
                if keyframe {
                    shared.keyframes.fetch_add(1, Ordering::AcqRel);
                    self.waiting_for_key = false;
                }
                shared.meter.lock().record(now, len);
                Flow::Continue
            }
            Err(TrySendError::Full(_)) => {
                // Deltas after a gap would corrupt the picture: skip to a
                // keyframe, forced through the same 500 ms gate as requests.
                shared.dropped.fetch_add(1, Ordering::AcqRel);
                self.waiting_for_key = true;
                self.gate.request();
                Flow::Continue
            }
            Err(TrySendError::Closed(_)) => Flow::ReceiverClosed,
        }
    }
}

/// An open hardware encoder and the parameters it was opened with.
struct ActiveEncoder {
    encoder: Encoder,
    size: Size,
    fps: u32,
    color: Option<Color>,
    bitrate: u32,
    can_retune: bool,
    can_cut: bool,
    /// Drain the MFT before releasing it (see the `Drop` impl).
    drain_on_drop: bool,
}

impl Drop for ActiveEncoder {
    /// Releasing a hardware MFT with work in flight let a Media Foundation
    /// work-queue callback run against freed state: an access violation in
    /// `RtwqLockSharedWorkQueue` in about 1 of 45 encoder teardowns on the dev
    /// machine (5 of 225), none in 225 when drained first. moq-video 0.1.23
    /// does not shut the async MFT down, so drain it here. Costs a few ms at
    /// stop, resize and tier changes; the drained tail is discarded (the next
    /// encoder starts with a keyframe).
    fn drop(&mut self) {
        if self.drain_on_drop
            && let Err(err) = self.encoder.flush()
        {
            tracing::debug!(%err, "screen encoder drain before release failed");
        }
    }
}

impl ActiveEncoder {
    fn open(size: Size, fps: u32, bitrate: u32, color: Option<Color>) -> Result<Self> {
        let rate = Rate::new(fps, 1).map_err(|e| Error::Encoder(e.to_string()))?;
        let mut config = EncoderConfig::new(size.width, size.height, rate);
        config.bitrate = Some(moq_net::bandwidth::Rate::from_bps(u64::from(bitrate)));
        // Media Foundation counts this in frames; the loop cuts every GOP of
        // wall time, so this is only a backstop if a forced keyframe is ignored.
        config.gop = Gop::keyframe_every(GOP * 2, rate);
        config.kind = Kind::Hardware;
        // Label the bitstream with the capture's own matrix, so a downscale
        // across the SD/HD boundary does not mislabel the colors.
        config.color = color;
        let encoder = Encoder::new(&config).map_err(Error::from_encoder)?;
        tracing::info!(
            encoder = encoder.name(),
            width = size.width,
            height = size.height,
            fps,
            bitrate,
            "screen encoder open"
        );
        Ok(Self {
            encoder,
            size,
            fps,
            color,
            bitrate,
            can_retune: true,
            can_cut: true,
            drain_on_drop: true,
        })
    }

    /// Apply a new target bitrate; a backend that refuses keeps its rate.
    fn retune(&mut self, bitrate: u32) {
        if bitrate == self.bitrate || !self.can_retune {
            return;
        }
        match self
            .encoder
            .set_bitrate(moq_net::bandwidth::Rate::from_bps(u64::from(bitrate)))
        {
            Ok(()) => self.bitrate = bitrate,
            Err(err) => {
                tracing::warn!(%err, "screen encoder cannot change bitrate; keeping its rate");
                self.can_retune = false;
            }
        }
    }

    /// Force a keyframe at the next frame. False if the backend cannot.
    fn cut(&mut self) -> bool {
        if !self.can_cut {
            return false;
        }
        match self.encoder.cut() {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(%err, "screen encoder cannot force keyframes");
                self.can_cut = false;
                false
            }
        }
    }
}
