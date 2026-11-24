//! The audio engine of this profile: creation on first use, settings, the event forwarder
//! (speaking, mic level, device problems), recovery after its thread dies, the synthetic
//! test microphone and clip decoding that cannot take the app down.
//!
//! Death detection: the engine's event sender lives only on the engine thread
//! (`tincan_audio` keeps it in the processor), so the forwarder's `recv()` returning `None`
//! outside shutdown means the thread is gone (a panic unwound it). The app then starts a
//! fresh engine and re-attaches the current session, at most [`MAX_RESTARTS`] times per
//! [`RESTART_WINDOW`].

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tincan_audio::{AudioConfig, AudioEngine, AudioError, AudioEvent, AudioIo, AudioSource, ClipPcm, NullSink};
use tincan_core::media::{KIND_VOICE, track_kind, track_short_id};
use tokio::sync::mpsc;

use super::{MAX_CLIP_MS, Media, State};
use crate::app::Core;
use crate::dto::{self, ToastKind};
use crate::emit::Ev;

/// Engine restarts allowed within [`RESTART_WINDOW`] before the app gives up and asks the
/// user to rejoin.
pub(super) const MAX_RESTARTS: usize = 3;
/// See [`MAX_RESTARTS`].
pub(super) const RESTART_WINDOW: Duration = Duration::from_secs(60);

/// The engine handle and its generation (each new engine gets a new one).
#[derive(Default)]
pub(crate) struct EngineSlot {
    /// The running engine, if any.
    pub engine: Option<AudioEngine>,
    /// Generation of `engine`.
    generation: u64,
    /// When engines died recently.
    deaths: VecDeque<Instant>,
}

impl EngineSlot {
    /// Record a death at `now`; true when engines died more than [`MAX_RESTARTS`] times
    /// within [`RESTART_WINDOW`].
    fn note_death(&mut self, now: Instant) -> bool {
        self.deaths.push_back(now);
        while self.deaths.front().is_some_and(|t| now.duration_since(*t) > RESTART_WINDOW) {
            self.deaths.pop_front();
        }
        self.deaths.len() > MAX_RESTARTS
    }

    /// Take out an engine whose thread stopped (it panicked), counting the death, so the
    /// next use starts a fresh engine even before the forwarder sees the closed event
    /// channel. Returns the dead handle (drop it off the async runtime: that joins its
    /// thread) and whether engines keep dying.
    pub(super) fn take_dead(&mut self, now: Instant) -> Option<(AudioEngine, bool)> {
        if self.engine.as_ref()?.is_running() {
            return None;
        }
        let give_up = self.note_death(now);
        self.engine.take().map(|e| (e, give_up))
    }

    /// The engine, if its thread runs.
    pub(super) fn running(&self) -> Option<AudioEngine> { self.engine.clone().filter(AudioEngine::is_running) }
}

/// Switches of the synthetic microphone used with `TestOptions::no_devices`.
#[derive(Debug, Default)]
pub(crate) struct TestAudio {
    /// Send speech-like bursts instead of silence.
    pub voice: AtomicBool,
    /// Panic on the next read (simulates the engine thread dying).
    pub crash: AtomicBool,
    /// Starting call audio fails (simulates an engine that cannot start).
    pub fail_start: AtomicBool,
}

/// 600 ms of speech-like sound, then 400 ms of silence (48 kHz samples).
const BURST: usize = 600 * 48;
const PERIOD: usize = 1000 * 48;

/// A speech-like signal: pitch-modulated harmonics with a syllable envelope.
fn speech(i: usize) -> f32 {
    let t = i as f32 / 48_000.0;
    let tau = 2.0 * std::f32::consts::PI;
    let phase = tau * (150.0 * t - 25.0 / (tau * 2.5) * (tau * 2.5 * t).cos());
    let env = 0.6 + 0.4 * (tau * 4.0 * t).sin();
    0.08 * env * (phase.sin() + 0.5 * (2.0 * phase).sin() + 0.25 * (3.0 * phase).sin())
}

/// The microphone of a `no_devices` profile: silence, or bursts when the test asks.
struct TestSource {
    pos: usize,
    ctl: Arc<TestAudio>,
}

impl AudioSource for TestSource {
    fn read(&mut self, frame: &mut [f32]) {
        if self.ctl.crash.swap(false, Ordering::AcqRel) {
            panic!("test hook: simulated audio engine crash");
        }
        if !self.ctl.voice.load(Ordering::Acquire) {
            frame.fill(0.0);
            return;
        }
        for s in frame.iter_mut() {
            *s = if self.pos % PERIOD < BURST { speech(self.pos) } else { 0.0 };
            self.pos = self.pos.wrapping_add(1);
        }
    }
}

/// Engine construction settings from the app settings.
fn audio_config(s: &dto::Settings, no_devices: bool) -> AudioConfig {
    AudioConfig {
        input_device: if no_devices { None } else { s.input_device_id.clone() },
        output_device: if no_devices { None } else { s.output_device_id.clone() },
        echo_cancellation: s.echo_cancellation,
        noise_suppression: s.noise_suppression,
        ai_noise_suppression: s.ai_noise_suppression,
        input_gain: s.input_volume as f32,
        output_gain: s.output_volume as f32,
        ..AudioConfig::default()
    }
}

/// Apply changed settings to a running engine.
pub(super) fn apply_settings(e: &AudioEngine, s: &dto::Settings, no_devices: bool) {
    if !no_devices && let Err(err) = e.set_devices(s.input_device_id.clone(), s.output_device_id.clone()) {
        tracing::debug!(error = %err, "audio devices not applied");
    }
    e.set_processing(s.echo_cancellation, s.noise_suppression, s.ai_noise_suppression);
    e.set_input_gain(s.input_volume as f32);
    e.set_output_gain(s.output_volume as f32);
}

/// Why a clip could not be decoded.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DecodeError {
    /// Longer than the limit.
    TooLong,
    /// Not audio, unsupported, corrupt, or the decoder panicked.
    Invalid,
}

/// Decode a clip on a blocking thread. symphonia runs on untrusted input (clips pulled from
/// peers), so a panic inside it is caught and reported as [`DecodeError::Invalid`].
pub(super) async fn decode_clip(bytes: Bytes) -> Result<Arc<ClipPcm>, DecodeError> {
    let joined = tokio::task::spawn_blocking(move || std::panic::catch_unwind(AssertUnwindSafe(|| AudioEngine::decode_clip(&bytes, MAX_CLIP_MS)))).await;
    match joined {
        Ok(Ok(Ok(pcm))) => Ok(Arc::new(pcm)),
        Ok(Ok(Err(AudioError::ClipTooLong { .. }))) => Err(DecodeError::TooLong),
        Ok(Ok(Err(e))) => {
            tracing::debug!(error = %e, "clip decode failed");
            Err(DecodeError::Invalid)
        }
        Ok(Err(_)) | Err(_) => {
            tracing::warn!("the clip decoder panicked; treating the clip as invalid");
            Err(DecodeError::Invalid)
        }
    }
}

impl Media {
    /// The running engine, if any (`None` once its thread stopped; the next
    /// [`Media::ensure_engine`] or the forwarder replaces it).
    pub(super) fn engine_handle(&self) -> Option<AudioEngine> { self.engine.lock().running() }

    /// The engine, created (with the current settings, mute state and volumes) if needed.
    /// Called with `st` locked (lock order: `st`, then the engine slot).
    pub(super) fn ensure_engine(&self, core: &Arc<Core>, st: &State) -> Result<AudioEngine, String> {
        let mut slot = self.engine.lock();
        let replaced = match slot.take_dead(Instant::now()) {
            Some((dead, give_up)) => {
                tracing::error!(generation = slot.generation, "the audio engine stopped; replacing it");
                std::thread::spawn(move || drop(dead));
                if give_up {
                    return Err("Audio keeps stopping. Leave voice and join again, or restart Tincan.".to_string());
                }
                true
            }
            None => false,
        };
        if let Some(e) = &slot.engine {
            return Ok(e.clone());
        }
        let cfg = audio_config(&core.settings.read(), self.no_devices);
        let (engine, events) = AudioEngine::new(cfg).map_err(|e| {
            tracing::error!(error = %e, "starting the audio engine failed");
            "Couldn't start audio. Restart Tincan and try again.".to_string()
        })?;
        if self.no_devices {
            engine.set_io(AudioIo::Custom { source: Box::new(TestSource { pos: 0, ctl: self.test.clone() }), sink: Box::new(NullSink) });
        }
        engine.set_muted(st.muted);
        engine.set_deafened(st.deafened);
        if st.mic_test {
            engine.mic_test(true);
        }
        // A new generation: the old forwarder's death report is ignored.
        slot.generation += 1;
        let generation = slot.generation;
        slot.engine = Some(engine.clone());
        drop(slot);
        self.apply_volumes(&engine, st);
        if replaced && let Some(a) = &st.attached {
            // What `engine_died` would have done for the session.
            if let Err(e) = engine.start_call(a.local_track, a.epoch, a.audio_out.clone()) {
                tracing::error!(error = %e, "restarting the call audio failed");
            }
            engine.set_relayed(a.relayed);
            self.counters.engine_restarts.fetch_add(1, Ordering::Relaxed);
        }
        tokio::spawn(forward(Arc::downgrade(core), events, generation));
        tracing::info!(generation, "audio engine started");
        Ok(engine)
    }

    /// One engine event.
    fn on_audio_event(&self, core: &Core, ev: AudioEvent) {
        match ev {
            // Only voice tracks light the ring (a soundboard track never sets the flag).
            AudioEvent::Speaking { track, speaking } if track_kind(track) == KIND_VOICE => {
                let mut st = self.st.lock();
                if let Some(user) = st.room.short.get(&track_short_id(track)).copied() {
                    set_speaking(core, &mut st, user, speaking);
                }
            }
            AudioEvent::LocalSpeaking(speaking) => {
                let mut st = self.st.lock();
                set_speaking(core, &mut st, core.me, speaking);
            }
            AudioEvent::MicLevel(level) => core.events.emit(Ev::MicLevel, dto::MicLevel { level: level.clamp(0.0, 1.0) }),
            AudioEvent::DeviceError(text) => core.toast(ToastKind::Error, text),
            AudioEvent::Speaking { .. } | AudioEvent::DeviceChanged => {}
            AudioEvent::ClipFinished(id) => {
                let mut st = self.st.lock();
                if st.playing.remove(&id).is_some() {
                    self.emit_voice(core, &mut st);
                }
            }
        }
    }

    /// The engine of `generation` stopped on its own: restart it and re-attach the session,
    /// or tell the user when it keeps dying.
    fn engine_died(&self, core: &Arc<Core>, generation: u64) {
        if self.is_closed() {
            return;
        }
        let mut st = self.st.lock();
        let (dead, give_up) = {
            let mut slot = self.engine.lock();
            if slot.generation != generation || slot.engine.is_none() {
                return;
            }
            let give_up = slot.note_death(Instant::now());
            (slot.engine.take(), give_up)
        };
        tracing::error!(generation, "the audio engine stopped unexpectedly");
        // The dead thread already exited; dropping the handle only joins it.
        if let Some(e) = dead {
            tokio::task::spawn_blocking(move || drop(e));
        }
        for user in std::mem::take(&mut st.speaking) {
            core.events.emit(Ev::Speaking, dto::Speaking { user_id: user.to_z32(), speaking: false });
        }
        // Clip ids belong to the dead engine, and its clips stopped with it.
        if !st.playing.is_empty() {
            st.playing.clear();
            self.emit_voice(core, &mut st);
        }
        if st.attached.is_none() && !st.mic_test {
            // Nothing is using audio; the next use starts a fresh engine.
            return;
        }
        if give_up {
            core.toast(ToastKind::Error, "Audio keeps stopping. Leave voice and join again, or restart Tincan.");
            return;
        }
        match self.ensure_engine(core, &st) {
            Ok(engine) => {
                if let Some(a) = &st.attached {
                    if let Err(e) = engine.start_call(a.local_track, a.epoch, a.audio_out.clone()) {
                        tracing::error!(error = %e, "restarting the call audio failed");
                    }
                    engine.set_relayed(a.relayed);
                }
                self.counters.engine_restarts.fetch_add(1, Ordering::Relaxed);
                core.toast(ToastKind::Error, "Audio stopped working, so Tincan restarted it.");
            }
            Err(e) => core.toast(ToastKind::Error, e),
        }
    }

    /// Start the engine's call side for a session. Retries once on a fresh engine if the
    /// current one turned out to be dead.
    pub(super) fn start_engine_call(&self, core: &Arc<Core>, st: &State, track: u16, epoch: u8, out: &mpsc::Sender<Bytes>) -> Result<(), String> {
        if self.test.fail_start.load(Ordering::Acquire) {
            return Err("Couldn't start audio. Try again.".to_string());
        }
        let engine = self.ensure_engine(core, st)?;
        match engine.start_call(track, epoch, out.clone()) {
            Ok(()) => Ok(()),
            Err(AudioError::EngineStopped) => {
                // Its forwarder has not noticed yet; replace it now.
                self.engine.lock().engine = None;
                let engine = self.ensure_engine(core, st)?;
                engine.start_call(track, epoch, out.clone()).map_err(|e| {
                    tracing::error!(error = %e, "starting call audio failed");
                    "Couldn't start audio. Try again.".to_string()
                })
            }
            Err(e) => {
                tracing::error!(error = %e, "starting call audio failed");
                Err("Couldn't start audio. Try again.".to_string())
            }
        }
    }
}

/// Record a speaking change and emit `speaking` when it changed.
pub(super) fn set_speaking(core: &Core, st: &mut State, user: tincan_core::UserId, speaking: bool) {
    let changed = if speaking { st.speaking.insert(user) } else { st.speaking.remove(&user) };
    if changed {
        core.events.emit(Ev::Speaking, dto::Speaking { user_id: user.to_z32(), speaking });
    }
}

/// Forward engine events until the engine thread ends.
async fn forward(core: Weak<Core>, mut events: mpsc::UnboundedReceiver<AudioEvent>, generation: u64) {
    while let Some(ev) = events.recv().await {
        let Some(core) = core.upgrade() else { return };
        core.media.on_audio_event(&core, ev);
    }
    if let Some(core) = core.upgrade() {
        core.media.engine_died(&core, generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tincan_core::media::AudioHeader;

    /// A 16-bit mono PCM WAV of `ms` milliseconds of a 440 Hz tone.
    fn wav(ms: u32, rate: u32) -> Vec<u8> {
        let n = (rate as u64 * ms as u64 / 1000) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + n * 2).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(n * 2).to_le_bytes());
        for i in 0..n {
            let v = ((i as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 8000.0) as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    }

    #[tokio::test]
    async fn decode_limits_and_garbage() {
        let pcm = decode_clip(Bytes::from(wav(2000, 48_000))).await.expect("2 s decodes");
        assert!((1900..=2100).contains(&pcm.duration_ms()));
        assert_eq!(decode_clip(Bytes::from(wav(21_000, 8_000))).await.err(), Some(DecodeError::TooLong));
        let mut x = 7u32;
        let junk: Vec<u8> = (0..20_000)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (x >> 16) as u8
            })
            .collect();
        assert_eq!(decode_clip(Bytes::from(junk)).await.err(), Some(DecodeError::Invalid));
        // A WAV header that promises far more data than it has: either result, no panic.
        let mut cut = wav(2000, 48_000);
        cut.truncate(100);
        let _ = decode_clip(Bytes::from(cut)).await;
        // A valid header followed by noise.
        let mut noisy = wav(500, 48_000);
        for (i, b) in noisy.iter_mut().enumerate().skip(44) {
            *b = (i as u8).wrapping_mul(31);
        }
        assert!(decode_clip(Bytes::from(noisy)).await.is_ok(), "PCM noise is still audio");
    }

    /// The Opus decoder the engine uses, driven directly with random payloads and with real
    /// packets that were bit-flipped, truncated or extended (what a broken or hostile peer
    /// can send): no panic. Errors are fine; the engine conceals them.
    #[test]
    fn opus_decoder_survives_malformed_packets() {
        use tincan_audio::{EncoderSettings, LibOpus, OpusCodec};
        let codec = LibOpus;
        let mut enc = codec.encoder(&EncoderSettings::default()).unwrap();
        let mut real = Vec::new();
        let (mut pcm, mut buf) = (vec![0f32; 960], vec![0u8; 1500]);
        for f in 0..60 {
            for (i, s) in pcm.iter_mut().enumerate() {
                *s = speech(f * 960 + i) * 4.0;
            }
            let n = enc.encode(&pcm, &mut buf).unwrap();
            if n > 2 {
                real.push(buf[..n].to_vec());
            }
        }
        assert!(real.len() > 30, "the encoder produced packets");
        let mut x = 0x5eed_u64;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut dec = codec.decoder().unwrap();
        let mut out = vec![0f32; 960];
        let mut panics = 0;
        for i in 0..6000u64 {
            let p: Vec<u8> = match i % 3 {
                0 => (0..1 + rnd() % 1275).map(|_| rnd() as u8).collect(),
                1 => {
                    let mut p = real[(rnd() % real.len() as u64) as usize].clone();
                    for _ in 0..1 + rnd() % 8 {
                        let at = (rnd() % p.len() as u64) as usize;
                        p[at] ^= 1 << (rnd() % 8);
                    }
                    p
                }
                _ => {
                    let mut p = real[(rnd() % real.len() as u64) as usize].clone();
                    let len = 1 + (rnd() % (p.len() as u64 * 2)) as usize;
                    p.resize(len, rnd() as u8);
                    p
                }
            };
            let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
                let _ = dec.decode(&p, &mut out);
                let _ = dec.decode_fec(&p, &mut out);
                if i % 7 == 0 {
                    let _ = dec.conceal(&mut out);
                }
            }));
            if r.is_err() {
                panics += 1;
                dec = codec.decoder().unwrap();
            }
            assert!(out.iter().all(|s| s.is_finite()), "decoder output stays finite");
        }
        assert_eq!(panics, 0, "the Opus decoder panicked on malformed input");
    }

    /// Q7: an engine whose thread died is noticed through `is_running`, without waiting for
    /// its event channel: the slot hands it out for replacement and counts the death.
    #[test]
    fn a_dead_engine_is_taken_out_of_the_slot() {
        let (engine, _events) = AudioEngine::new(AudioConfig::default()).unwrap();
        let ctl = Arc::new(TestAudio::default());
        engine.set_io(AudioIo::Custom { source: Box::new(TestSource { pos: 0, ctl: ctl.clone() }), sink: Box::new(NullSink) });
        let mut slot = EngineSlot { engine: Some(engine.clone()), ..EngineSlot::default() };
        assert!(slot.take_dead(Instant::now()).is_none(), "a running engine stays");
        assert!(slot.running().is_some());
        ctl.crash.store(true, Ordering::Release);
        engine.mic_test(true);
        let deadline = Instant::now() + Duration::from_secs(5);
        while engine.is_running() {
            assert!(Instant::now() < deadline, "the engine thread did not stop");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(slot.running().is_none(), "a stopped engine is not handed out");
        let (dead, give_up) = slot.take_dead(Instant::now()).expect("the dead engine is taken out");
        assert!(!give_up);
        assert!(slot.engine.is_none());
        assert_eq!(slot.deaths.len(), 1);
        drop(dead);
        // Past MAX_RESTARTS deaths in the window, the slot says to give up.
        let now = Instant::now();
        for _ in 0..MAX_RESTARTS {
            slot.note_death(now);
        }
        assert!(slot.note_death(now));
    }

    /// Malformed Opus payloads from a peer must not kill the engine thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn garbage_packets_do_not_kill_the_engine() {
        let (engine, mut events) = AudioEngine::new(AudioConfig::default()).unwrap();
        engine.set_io(AudioIo::Custom { source: Box::new(tincan_audio::SilenceSource), sink: Box::new(NullSink) });
        let (tx, _rx) = mpsc::channel(64);
        engine.start_call(4, 1, tx).unwrap();
        let mut x = 99u64;
        let mut next = move || {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (x >> 33) as u32
        };
        for track in [8u16, 12, 16] {
            for seq in 0..400u16 {
                let len = 1 + (next() % 1200) as usize;
                let payload: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                let flags = (next() & 0x0f) as u8;
                let pkt = AudioHeader { flags, track, seq, ts: u32::from(seq) * 960, level: 10, epoch: 1 }.packet(&payload);
                engine.push_packet(pkt);
                if seq % 50 == 0 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        // A dead engine thread refuses commands and closes its event channel.
        assert!(engine.set_devices(None, None).is_ok(), "the engine thread survived malformed packets");
        while let Ok(ev) = events.try_recv() {
            let _ = ev;
        }
        assert!(!events.is_closed(), "the engine's event channel is still open");
        engine.stop_call();
    }
}
