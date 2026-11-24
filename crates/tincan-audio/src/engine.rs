//! The engine thread and its public handle.
//!
//! One thread (`tincan-audio`) owns every piece of audio state and every cpal stream. The
//! [`AudioEngine`] handle sends commands over a channel and unparks the thread. The thread is
//! woken by device callbacks (input data arrived, output wants data), by commands, and by a
//! 10 ms clock only when it is needed (custom IO, or a call whose device is missing). With no
//! call, no mic test and no UI sound it sleeps without a timeout.

use crate::clip::ClipPcm;
use crate::codec::{EncoderSettings, OpusCodec};
use crate::device::{self, Dir, ENGINE_RATE, Fault, InputDevice, OutputDevice};
use crate::error::{AudioError, Result};
use crate::jitter::JitterConfig;
use crate::processor::{FRAME_10MS, Processor, Settings};
use crate::{AudioConfig, AudioDevices, AudioEvent, AudioIo, AudioSink, AudioSource};
use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const TICK: Duration = Duration::from_millis(10);
/// The clock skips ahead instead of catching up after a stall longer than this.
const MAX_CATCH_UP: Duration = Duration::from_millis(200);
/// Retry interval for a device that failed to open.
const RETRY: Duration = Duration::from_secs(2);
/// The output stays open this long after the last UI sound, so chimes in a row reuse it.
const OUTPUT_LINGER: Duration = Duration::from_millis(500);
/// Received datagrams queued for the engine thread; more are dropped.
const PACKET_QUEUE: usize = 1024;

enum Cmd {
    SetDevices { input: Option<String>, output: Option<String> },
    SetIo(AudioIo),
    StartCall { track: u16, epoch: u8, out: mpsc::Sender<Bytes>, enc: Box<dyn crate::codec::OpusEncode> },
    StopCall,
    SetEpoch(u8),
    RemoveTrack(u16),
    Muted(bool),
    Deafened(bool),
    TrackVolume(u16, f32),
    InputGain(f32),
    OutputGain(f32),
    Processing { echo_cancellation: bool, noise_suppression: bool },
    LossHint(u8),
    Relayed(bool),
    PlayClip(Arc<ClipPcm>, u64),
    StopClip(u64),
    StopClips,
    LocalSoundGain(f32),
    PlayLocal(Arc<ClipPcm>),
    MicTest(bool),
    Shutdown,
}

struct Shared {
    in_call: AtomicBool,
    deafened: AtomicBool,
    alive: AtomicBool,
    /// Last soundboard clip id handed out.
    clip_id: AtomicU64,
}

/// Runs the engine thread above normal priority so playout keeps up under CPU load.
#[cfg(windows)]
fn raise_thread_priority() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST};
    // SAFETY: GetCurrentThread returns a pseudo-handle for the calling thread that needs no
    // closing; SetThreadPriority only changes the scheduling priority of that thread.
    if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) } {
        tracing::debug!(%e, "could not raise the audio thread priority");
    }
}

#[cfg(not(windows))]
fn raise_thread_priority() {}

struct AliveGuard(Arc<Shared>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::Release);
        self.0.in_call.store(false, Ordering::Release);
    }
}

struct Inner {
    cmd: Sender<Cmd>,
    pkt: Sender<(Instant, Bytes)>,
    waker: Thread,
    shared: Arc<Shared>,
    codec: Arc<dyn OpusCodec>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Shutdown);
        self.waker.unpark();
        if let Some(j) = self.join.lock().take()
            && j.join().is_err()
        {
            tracing::error!("audio engine thread panicked");
        }
    }
}

/// Handle to the native audio engine. Cheap to clone, `Send + Sync`. The engine thread stops
/// when the last clone is dropped.
///
/// All methods are non-blocking: they queue a command for the engine thread (except
/// [`AudioEngine::devices`] and [`AudioEngine::decode_clip`], which do their work in the caller).
#[derive(Clone)]
pub struct AudioEngine {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for AudioEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEngine")
            .field("in_call", &self.inner.shared.in_call.load(Ordering::Relaxed))
            .field("alive", &self.inner.shared.alive.load(Ordering::Relaxed))
            .finish()
    }
}

impl AudioEngine {
    /// Starts the engine thread. No device is opened until a call, a mic test or a UI sound
    /// needs one. Events arrive on the returned receiver.
    pub fn new(cfg: AudioConfig) -> Result<(AudioEngine, mpsc::UnboundedReceiver<AudioEvent>)> {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (pkt_tx, pkt_rx) = crossbeam_channel::bounded(PACKET_QUEUE);
        let shared = Arc::new(Shared { in_call: AtomicBool::new(false), deafened: AtomicBool::new(false), alive: AtomicBool::new(true), clip_id: AtomicU64::new(0) });
        let codec = cfg.codec.clone();
        let settings = Settings {
            echo_cancellation: cfg.echo_cancellation,
            noise_suppression: cfg.noise_suppression,
            input_gain: cfg.input_gain,
            output_gain: cfg.output_gain,
            jitter: cfg.jitter,
        };
        let thread_shared = shared.clone();
        let join = std::thread::Builder::new()
            .name("tincan-audio".into())
            .spawn(move || {
                // marks the engine stopped even if the thread unwinds
                let _alive = AliveGuard(thread_shared);
                raise_thread_priority();
                Driver::new(cfg, settings, ev_tx, cmd_rx, pkt_rx).run();
            })
            .map_err(|e| AudioError::Thread(e.to_string()))?;
        let waker = join.thread().clone();
        let inner = Inner { cmd: cmd_tx, pkt: pkt_tx, waker, shared, codec, join: Mutex::new(Some(join)) };
        Ok((AudioEngine { inner: Arc::new(inner) }, ev_rx))
    }

    fn send(&self, cmd: Cmd) -> Result<()> {
        if !self.inner.shared.alive.load(Ordering::Acquire) {
            return Err(AudioError::EngineStopped);
        }
        self.inner.cmd.send(cmd).map_err(|_| AudioError::EngineStopped)?;
        self.inner.waker.unpark();
        Ok(())
    }

    fn post(&self, cmd: Cmd) {
        if self.send(cmd).is_err() {
            tracing::debug!("audio engine stopped; command dropped");
        }
    }

    /// True while the engine thread runs. False once it has stopped: it panicked (a panic
    /// unwinds the thread and every later command fails with [`AudioError::EngineStopped`]),
    /// or the last handle is being dropped. A stopped engine never comes back; make a new one.
    pub fn is_running(&self) -> bool {
        self.inner.shared.alive.load(Ordering::Acquire)
    }

    /// Lists audio devices. Device ids are stable strings (cpal `DeviceId`, e.g. `wasapi:...`)
    /// to store in settings and pass to [`AudioEngine::set_devices`].
    pub fn devices() -> Result<AudioDevices> {
        device::enumerate()
    }

    /// Selects the input and output device (`None` = system default, which is followed when it
    /// changes). Open streams are switched immediately. A selected device that is missing falls
    /// back to the default and reports a `DeviceError`.
    pub fn set_devices(&self, input: Option<String>, output: Option<String>) -> Result<()> {
        self.send(Cmd::SetDevices { input, output })
    }

    /// Chooses real devices (default) or a custom source/sink driven by an internal 10 ms
    /// clock (tests, headless). Takes effect immediately.
    pub fn set_io(&self, io: AudioIo) {
        self.post(Cmd::SetIo(io));
    }

    /// Starts a call: opens the devices and starts capture -> APM -> Opus -> datagrams.
    /// Each datagram (12-byte `tincan_core::media::AudioHeader` + Opus) goes to `out` with
    /// `try_send`; it is dropped when the channel is full. `local_track` is our track id
    /// (`short_id * 4 + KIND_VOICE`), `epoch` the host epoch low byte. A running call is
    /// restarted (remote tracks are dropped). Device problems are reported as events, not errors.
    pub fn start_call(&self, local_track: u16, epoch: u8, out: mpsc::Sender<Bytes>) -> Result<()> {
        let enc = self.inner.codec.encoder(&EncoderSettings::default())?;
        self.send(Cmd::StartCall { track: local_track, epoch, out, enc })?;
        self.inner.shared.in_call.store(true, Ordering::Release);
        Ok(())
    }

    /// Updates the host epoch written into outgoing headers (after a host migration).
    pub fn set_epoch(&self, epoch: u8) {
        self.post(Cmd::SetEpoch(epoch));
    }

    /// Ends the call: stops sending, drops all remote tracks, closes the devices unless a mic
    /// test or a UI sound still uses them.
    pub fn stop_call(&self) {
        self.inner.shared.in_call.store(false, Ordering::Release);
        self.post(Cmd::StopCall);
    }

    /// Hands one received audio datagram to the engine (any track; routed by `header.track`).
    /// Non-blocking; the arrival time is taken here. Ignored when no call is running.
    pub fn push_packet(&self, pkt: Bytes) {
        if self.inner.shared.in_call.load(Ordering::Acquire) {
            let _ = self.inner.pkt.try_send((Instant::now(), pkt));
        }
    }

    /// Forgets a remote track (member left the room). Emits `Speaking{false}` if it was lit.
    pub fn remove_track(&self, track: u16) {
        self.post(Cmd::RemoveTrack(track));
    }

    /// Mute: stop sending voice (soundboard clips are still sent).
    pub fn set_muted(&self, muted: bool) {
        self.post(Cmd::Muted(muted));
    }

    /// Deafen: mute and stop playing call audio (UI sounds still play). Cancels soundboard clips.
    pub fn set_deafened(&self, deafened: bool) {
        self.inner.shared.deafened.store(deafened, Ordering::Release);
        self.post(Cmd::Deafened(deafened));
    }

    /// Per-track playback gain (0..=4, 1 = unchanged). Kept across track resets.
    pub fn set_track_volume(&self, track: u16, gain: f32) {
        self.post(Cmd::TrackVolume(track, gain));
    }

    /// Microphone gain applied before processing (0..=4, 1 = unchanged).
    pub fn set_input_gain(&self, g: f32) {
        self.post(Cmd::InputGain(g));
    }

    /// Speaker gain applied to the final mix (0..=4, 1 = unchanged).
    pub fn set_output_gain(&self, g: f32) {
        self.post(Cmd::OutputGain(g));
    }

    /// Turns echo cancellation and noise suppression on or off (AGC and high-pass stay on).
    pub fn set_processing(&self, echo_cancellation: bool, noise_suppression: bool) {
        self.post(Cmd::Processing { echo_cancellation, noise_suppression });
    }

    /// Receiver-reported packet loss in percent: sets Opus packet-loss-perc (clamped 5..=20)
    /// and drops the bitrate to 24 kbps above 10 % loss (32 kbps otherwise).
    pub fn set_loss_hint(&self, loss_pct: u8) {
        self.post(Cmd::LossHint(loss_pct));
    }

    /// Raises the jitter buffer maximum to 300 ms for relayed paths (200 ms otherwise).
    pub fn set_relayed(&self, relayed: bool) {
        self.post(Cmd::Relayed(relayed));
    }

    /// Decodes a soundboard clip (WAV, MP3, Ogg Vorbis, FLAC, M4A/AAC) to 48 kHz mono.
    /// Errors when it is longer than `max_ms`. Runs in the caller (CPU-bound, no IO).
    pub fn decode_clip(bytes: &[u8], max_ms: u32) -> Result<ClipPcm> {
        crate::clip::decode_clip(bytes, max_ms)
    }

    /// Plays a soundboard clip into the call and locally (so the echo canceller has it as
    /// reference). In the call it goes out on its own Opus stream, track
    /// `track_id(short_id, KIND_SOUND)` with the header flag `AF_SOUNDBOARD`, so receivers can
    /// set its volume apart from the voice; nothing is sent on that track between clips.
    /// Works while muted. Returns the clip's id; `AudioEvent::ClipFinished(id)` follows when it
    /// is over. Errors: [`AudioError::NotInCall`], [`AudioError::Deafened`].
    pub fn play_clip(&self, clip: Arc<ClipPcm>) -> Result<u64> {
        if !self.inner.shared.in_call.load(Ordering::Acquire) {
            return Err(AudioError::NotInCall);
        }
        if self.inner.shared.deafened.load(Ordering::Acquire) {
            return Err(AudioError::Deafened);
        }
        let id = self.inner.shared.clip_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.send(Cmd::PlayClip(clip, id))?;
        Ok(id)
    }

    /// Stops soundboard clip `id` locally and in the call at once (a 5 ms fade).
    /// `ClipFinished(id)` follows. Unknown or finished ids are ignored.
    pub fn stop_clip(&self, id: u64) {
        self.post(Cmd::StopClip(id));
    }

    /// Stops every soundboard clip I am playing (see [`AudioEngine::stop_clip`]).
    pub fn stop_clips(&self) {
        self.post(Cmd::StopClips);
    }

    /// Local gain of my own soundboard clips (0..=4, 1 = unchanged), such as the room's
    /// soundboard volume. What goes into the call is not changed.
    pub fn set_local_sound_gain(&self, g: f32) {
        self.post(Cmd::LocalSoundGain(g));
    }

    /// Plays a UI sound (join/leave chime) through the output mixer, opening the output if
    /// needed. Not sent to the call; plays while deafened.
    pub fn play_local(&self, clip: Arc<ClipPcm>) {
        self.post(Cmd::PlayLocal(clip));
    }

    /// Mic test: opens the input (only) and emits `MicLevel` about 20 times per second.
    pub fn mic_test(&self, on: bool) {
        self.post(Cmd::MicTest(on));
    }
}

enum Io {
    Devices,
    Custom { source: Box<dyn AudioSource>, sink: Box<dyn AudioSink> },
}

/// State of one device direction in device mode.
#[derive(Default)]
struct DevState {
    selected: Option<String>,
    failed: bool,
    retry_at: Option<Instant>,
}

struct Driver {
    proc: Processor,
    events: mpsc::UnboundedSender<AudioEvent>,
    cmd_rx: Receiver<Cmd>,
    pkt_rx: Receiver<(Instant, Bytes)>,
    fault_rx: Receiver<Fault>,
    fault_tx: Sender<Fault>,
    waker: Thread,
    t0: Instant,
    io: Io,
    input: Option<InputDevice>,
    output: Option<OutputDevice>,
    in_state: DevState,
    out_state: DevState,
    generation: u64,
    clock_next: Option<Instant>,
    last_output_need: Option<Instant>,
    fifo48: VecDeque<f32>,
    frame: Vec<f32>,
    render_buf: Vec<f32>,
    relayed: bool,
    jitter_base: JitterConfig,
}

impl Driver {
    fn new(
        cfg: AudioConfig,
        settings: Settings,
        events: mpsc::UnboundedSender<AudioEvent>,
        cmd_rx: Receiver<Cmd>,
        pkt_rx: Receiver<(Instant, Bytes)>,
    ) -> Self {
        let (fault_tx, fault_rx) = crossbeam_channel::bounded(16);
        Self {
            proc: Processor::new(cfg.codec.clone(), events.clone(), settings),
            events,
            cmd_rx,
            pkt_rx,
            fault_rx,
            fault_tx,
            waker: std::thread::current(),
            t0: Instant::now(),
            io: Io::Devices,
            input: None,
            output: None,
            in_state: DevState { selected: cfg.input_device, ..Default::default() },
            out_state: DevState { selected: cfg.output_device, ..Default::default() },
            generation: 0,
            clock_next: None,
            last_output_need: None,
            fifo48: VecDeque::with_capacity(ENGINE_RATE as usize / 5),
            frame: vec![0.0; FRAME_10MS],
            render_buf: vec![0.0; FRAME_10MS],
            relayed: false,
            jitter_base: cfg.jitter,
        }
    }

    fn emit(&self, ev: AudioEvent) {
        let _ = self.events.send(ev);
    }

    fn ms(&self, t: Instant) -> u64 {
        t.saturating_duration_since(self.t0).as_millis() as u64
    }

    fn run(mut self) {
        loop {
            if !self.drain_commands() {
                break;
            }
            let now = Instant::now();
            while let Ok(f) = self.fault_rx.try_recv() {
                self.handle_fault(f, now);
            }
            let mut pkts = 0;
            while pkts < PACKET_QUEUE {
                match self.pkt_rx.try_recv() {
                    Ok((at, pkt)) => {
                        let at = self.ms(at);
                        self.proc.push_packet(pkt, at);
                        pkts += 1;
                    }
                    Err(_) => break,
                }
            }
            if self.proc.in_call() || self.proc.has_local_voices() {
                self.last_output_need = Some(now);
            }
            if matches!(self.io, Io::Devices) {
                self.reconcile_devices(now);
            } else {
                self.close_devices();
            }
            let now_ms = self.ms(now);
            self.pump_output(now_ms);
            self.pump_input(now_ms);
            self.run_clock(now);
            self.proc.tick(self.ms(Instant::now()));
            self.park(now);
        }
        self.proc.stop_call();
        self.close_devices();
        tracing::debug!("audio engine thread exiting");
    }

    /// Handles queued commands. Returns false on shutdown.
    fn drain_commands(&mut self) -> bool {
        loop {
            match self.cmd_rx.try_recv() {
                Ok(Cmd::Shutdown) | Err(TryRecvError::Disconnected) => return false,
                Ok(cmd) => self.handle(cmd),
                Err(TryRecvError::Empty) => return true,
            }
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::SetDevices { input, output } => {
                if self.in_state.selected != input {
                    self.in_state = DevState { selected: input, ..Default::default() };
                    self.input = None;
                    self.fifo48.clear();
                }
                if self.out_state.selected != output {
                    self.out_state = DevState { selected: output, ..Default::default() };
                    self.output = None;
                }
            }
            Cmd::SetIo(io) => {
                self.close_devices();
                self.in_state.failed = false;
                self.out_state.failed = false;
                self.clock_next = None;
                self.io = match io {
                    AudioIo::Devices => Io::Devices,
                    AudioIo::Custom { source, sink } => Io::Custom { source, sink },
                };
            }
            Cmd::StartCall { track, epoch, out, enc } => self.proc.start_call(track, epoch, out, enc),
            Cmd::StopCall => {
                self.proc.stop_call();
                // drop packets queued for the old call
                while self.pkt_rx.try_recv().is_ok() {}
            }
            Cmd::SetEpoch(e) => self.proc.set_epoch(e),
            Cmd::RemoveTrack(t) => self.proc.remove_track(t),
            Cmd::Muted(m) => self.proc.set_muted(m),
            Cmd::Deafened(d) => self.proc.set_deafened(d),
            Cmd::TrackVolume(t, g) => self.proc.set_track_volume(t, g),
            Cmd::InputGain(g) => self.proc.set_input_gain(g),
            Cmd::OutputGain(g) => self.proc.set_output_gain(g),
            Cmd::Processing { echo_cancellation, noise_suppression } => {
                self.proc.set_processing(echo_cancellation, noise_suppression);
            }
            Cmd::LossHint(l) => self.proc.set_loss_hint(l),
            Cmd::Relayed(r) => {
                if r != self.relayed {
                    self.relayed = r;
                    let base = self.jitter_base;
                    let cfg = if r { JitterConfig { max_ms: base.max_ms.max(JitterConfig::relayed().max_ms), ..base } } else { base };
                    self.proc.set_jitter_config(cfg);
                }
            }
            Cmd::PlayClip(c, id) => {
                // A refused clip still gets its ClipFinished (from the processor).
                if let Err(e) = self.proc.play_clip(c, id) {
                    tracing::debug!(%e, "play_clip ignored");
                }
            }
            Cmd::StopClip(id) => self.proc.stop_clip(id),
            Cmd::StopClips => self.proc.stop_all_clips(),
            Cmd::LocalSoundGain(g) => self.proc.set_local_sound_gain(g),
            Cmd::PlayLocal(c) => self.proc.play_local(c),
            Cmd::MicTest(on) => self.proc.set_mic_test(on),
            Cmd::Shutdown => {}
        }
    }

    fn want_input(&self) -> bool {
        self.proc.in_call() || self.proc.mic_test()
    }

    fn want_output(&self, now: Instant) -> bool {
        self.proc.in_call()
            || self.proc.has_local_voices()
            || self.last_output_need.is_some_and(|t| now.saturating_duration_since(t) < OUTPUT_LINGER)
    }

    fn close_devices(&mut self) {
        if let Some(d) = self.input.take() {
            tracing::info!(device = %d.name, "microphone closed");
        }
        if let Some(d) = self.output.take() {
            tracing::info!(device = %d.name, "speaker closed");
        }
        self.fifo48.clear();
    }

    fn reconcile_devices(&mut self, now: Instant) {
        if self.want_input() {
            if self.input.is_none() && self.in_state.retry_at.is_none_or(|t| t <= now) {
                self.open(Dir::Input, now);
            }
        } else {
            if let Some(d) = self.input.take() {
                tracing::info!(device = %d.name, "microphone closed");
                self.fifo48.clear();
            }
            self.in_state.retry_at = None;
            self.in_state.failed = false;
        }
        if self.want_output(now) {
            if self.output.is_none() && self.out_state.retry_at.is_none_or(|t| t <= now) {
                self.open(Dir::Output, now);
            }
        } else {
            if let Some(d) = self.output.take() {
                tracing::info!(device = %d.name, "speaker closed");
            }
            self.out_state.retry_at = None;
            self.out_state.failed = false;
        }
    }

    fn open(&mut self, dir: Dir, now: Instant) {
        self.generation += 1;
        let generation = self.generation;
        let (waker, faults) = (self.waker.clone(), self.fault_tx.clone());
        let result = match dir {
            Dir::Input => device::open_input(self.in_state.selected.as_deref(), waker, faults, generation).map(|(d, m)| {
                self.input = Some(d);
                m
            }),
            Dir::Output => device::open_output(self.out_state.selected.as_deref(), waker, faults, generation).map(|(d, m)| {
                self.output = Some(d);
                m
            }),
        };
        let st = match dir {
            Dir::Input => &mut self.in_state,
            Dir::Output => &mut self.out_state,
        };
        match result {
            Ok(missing) => {
                let recovered = st.failed;
                st.failed = false;
                st.retry_at = None;
                if missing {
                    self.emit(AudioEvent::DeviceError(format!(
                        "The selected {} is not available; using the default.",
                        dir.noun()
                    )));
                }
                if recovered {
                    self.emit(AudioEvent::DeviceChanged);
                }
            }
            Err(e) => {
                let first = !st.failed;
                st.failed = true;
                st.retry_at = Some(now + RETRY);
                if first {
                    tracing::warn!(%e, ?dir, "audio device open failed");
                    self.emit(AudioEvent::DeviceError(format!("Couldn't open the {}: {e}", dir.noun())));
                }
            }
        }
    }

    fn handle_fault(&mut self, f: Fault, now: Instant) {
        let current = match f.dir {
            Dir::Input => self.input.as_ref().map(|d| d.generation),
            Dir::Output => self.output.as_ref().map(|d| d.generation),
        };
        if current != Some(f.generation) {
            return;
        }
        use cpal::ErrorKind as K;
        match f.kind {
            K::Xrun | K::RealtimeDenied => {
                tracing::debug!(kind = ?f.kind, dir = ?f.dir, "audio stream glitch");
                return;
            }
            K::DeviceChanged => {
                tracing::info!(dir = ?f.dir, "audio route changed");
                self.emit(AudioEvent::DeviceChanged);
                return;
            }
            _ => {}
        }
        // The stream is unusable: drop it and reopen (default device if the selected one is gone).
        let st = match f.dir {
            Dir::Input => {
                self.input = None;
                self.fifo48.clear();
                &mut self.in_state
            }
            Dir::Output => {
                self.output = None;
                &mut self.out_state
            }
        };
        match f.kind {
            K::StreamInvalidated => {
                // the default device changed: rebuild on the new default right away
                tracing::info!(dir = ?f.dir, "default audio device changed; reopening");
                st.retry_at = None;
                st.failed = true;
            }
            K::DeviceNotAvailable => {
                tracing::warn!(dir = ?f.dir, "audio device disappeared");
                st.retry_at = None;
                st.failed = true;
                self.emit(AudioEvent::DeviceError(format!("The {} was disconnected.", f.dir.noun())));
            }
            other => {
                tracing::warn!(kind = ?other, dir = ?f.dir, "audio stream failed");
                st.retry_at = Some(now + RETRY);
                st.failed = true;
                self.emit(AudioEvent::DeviceError(format!("The {} stopped working: {other}", f.dir.noun())));
            }
        }
    }

    fn update_aec_delay(&mut self) {
        let mut ms = 0u64;
        if let Some(o) = &self.output {
            ms += o.ring.len() as u64 * 1000 / u64::from(o.rate.max(1));
            ms += u64::from(o.latency_us.load(Ordering::Relaxed)) / 1000;
        }
        if let Some(i) = &self.input {
            ms += i.ring.len() as u64 * 1000 / u64::from(i.rate.max(1));
            ms += u64::from(i.latency_us.load(Ordering::Relaxed)) / 1000;
        }
        ms += self.fifo48.len() as u64 / 48;
        self.proc.set_aec_delay_ms(i32::try_from(ms).unwrap_or(500));
    }

    /// Tops the output ring up to one callback plus 15 ms.
    fn pump_output(&mut self, now_ms: u64) {
        let Some(out) = self.output.as_mut() else { return };
        let cb = (out.cb_frames.load(Ordering::Relaxed) as usize).max(out.rate as usize / 100);
        let target = (cb + out.rate as usize * 15 / 1000).min(out.ring.capacity() / 2);
        for _ in 0..32 {
            if out.ring.len() >= target {
                break;
            }
            self.proc.render(&mut self.render_buf, now_ms, true);
            match &mut out.resampler {
                None => {
                    out.ring.push_slice(&self.render_buf);
                }
                Some(rs) => {
                    out.pending.extend_from_slice(&self.render_buf);
                    loop {
                        let need = rs.input_frames_next();
                        if out.pending.len() < need {
                            break;
                        }
                        match rs.process(&out.pending[..need]) {
                            Ok(o) => {
                                out.ring.push_slice(o);
                            }
                            Err(e) => tracing::debug!(%e, "output resample"),
                        }
                        out.pending.drain(..need);
                    }
                }
            }
        }
    }

    /// Moves captured audio through the resampler into 10 ms frames for the processor.
    fn pump_input(&mut self, now_ms: u64) {
        if self.input.is_none() {
            return;
        }
        self.update_aec_delay();
        let Some(inp) = self.input.as_mut() else { return };
        // after a stall, drop the backlog instead of sending stale audio
        let backlog = inp.ring.len();
        let keep = inp.rate as usize / 50;
        if backlog > inp.rate as usize / 5 {
            inp.ring.skip(backlog - keep);
        }
        loop {
            let need = inp.resampler.as_ref().map_or(FRAME_10MS, |r| r.input_frames_next());
            if inp.ring.len() < need || need > inp.scratch.len() {
                break;
            }
            let n = inp.ring.pop_slice(&mut inp.scratch[..need]);
            match &mut inp.resampler {
                None => self.fifo48.extend(&inp.scratch[..n]),
                Some(rs) => match rs.process(&inp.scratch[..n]) {
                    Ok(o) => self.fifo48.extend(o),
                    Err(e) => tracing::debug!(%e, "input resample"),
                },
            }
        }
        while self.fifo48.len() >= FRAME_10MS {
            for (d, s) in self.frame.iter_mut().zip(self.fifo48.drain(..FRAME_10MS)) {
                *d = s;
            }
            self.proc.capture(&mut self.frame, now_ms);
        }
    }

    /// Which sides the internal 10 ms clock drives: (render, capture).
    fn clock_sides(&self, now: Instant) -> (bool, bool) {
        match self.io {
            Io::Custom { .. } => (self.want_output(now), self.want_input()),
            Io::Devices => {
                // keep the call (and UI sounds) flowing when a device is missing
                let call = self.proc.in_call();
                let render = self.output.is_none() && (call || self.proc.has_local_voices());
                (render, call && self.input.is_none())
            }
        }
    }

    fn run_clock(&mut self, now: Instant) {
        let (render, capture) = self.clock_sides(now);
        if !render && !capture {
            self.clock_next = None;
            return;
        }
        let mut next = self.clock_next.unwrap_or(now);
        if now.saturating_duration_since(next) > MAX_CATCH_UP {
            next = now;
        }
        while next <= now {
            let ms = self.ms(next);
            let custom = matches!(self.io, Io::Custom { .. });
            if render {
                self.proc.render(&mut self.render_buf, ms, custom);
                if let Io::Custom { sink, .. } = &mut self.io {
                    sink.write(&self.render_buf);
                }
            }
            if capture {
                match &mut self.io {
                    Io::Custom { source, .. } => source.read(&mut self.frame),
                    Io::Devices => self.frame.fill(0.0),
                }
                self.proc.capture(&mut self.frame, ms);
            }
            next += TICK;
        }
        self.clock_next = Some(next);
    }

    fn park(&self, now: Instant) {
        let mut deadline = self.clock_next;
        let mut consider = |t: Option<Instant>| {
            if let Some(t) = t {
                deadline = Some(deadline.map_or(t, |d| d.min(t)));
            }
        };
        if matches!(self.io, Io::Devices) {
            if self.want_input() && self.input.is_none() {
                consider(self.in_state.retry_at);
            }
            if self.output.is_some() || self.want_output(now) {
                if self.output.is_none() {
                    consider(self.out_state.retry_at);
                }
                consider(self.last_output_need.map(|t| t + OUTPUT_LINGER));
            }
        }
        match deadline {
            Some(d) => {
                let wait = d.saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    std::thread::park_timeout(wait);
                }
            }
            None => std::thread::park(),
        }
    }
}
