//! The engine core: everything between 10 ms PCM frames and audio datagrams.
//!
//! [`Processor`] owns the APM, the Opus encoder, one jitter buffer and decoder per remote track,
//! the mixer and the clip voices. It has no threads, devices or clocks of its own: the driver in
//! `engine.rs` calls [`Processor::capture`] with each 10 ms of microphone audio,
//! [`Processor::render`] whenever the speaker needs 10 ms more, and [`Processor::push_packet`]
//! for every received datagram, always passing the current time. That keeps it deterministic and
//! testable in virtual time.

use crate::AudioEvent;
use crate::apm::{APM_FRAME, Apm};
use crate::clip::ClipPcm;
use crate::codec::{MAX_OPUS_PACKET, OpusCodec, OpusDecode, OpusEncode, packet_has_silk};
use crate::dsp::{LocalVad, RemoteSpeaking, dbov, fade_in, fade_out, level_byte, meter_level, rms, soft_limit, soft_limit_all};
use crate::error::{AudioError, Result};
use crate::jitter::{FRAME_MS, FRAME_SAMPLES, JitterBuffer, JitterConfig, Playout};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use tincan_core::media::{AF_FEC, AF_SOUNDBOARD, AF_SPEAKING, AF_TALKSPURT, AUDIO_HEADER_LEN, AudioHeader};
use tokio::sync::mpsc;

/// Samples per processing frame (10 ms at 48 kHz).
pub(crate) const FRAME_10MS: usize = APM_FRAME;
/// Remote tracks kept at once; packets for further track ids are ignored.
pub(crate) const MAX_TRACKS: usize = 32;
/// Clip voices playing at once; the oldest is cut when another starts.
pub(crate) const MAX_VOICES: usize = 8;
/// Gain applied to soundboard clips (in the call and locally), -3 dB.
pub(crate) const SOUNDBOARD_GAIN: f32 = 0.7;
/// Fade-in length after silence or concealment, to avoid clicks (5 ms).
const FADE_IN: usize = 240;
/// Capture frames per `MicLevel` event (50 ms -> 20 events per second).
const METER_FRAMES: u32 = 5;

/// Runtime settings of the processor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Settings {
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub input_gain: f32,
    pub output_gain: f32,
    pub jitter: JitterConfig,
}

struct CallState {
    track: u16,
    epoch: u8,
    out: mpsc::Sender<Bytes>,
    enc: Box<dyn OpusEncode>,
    apm: Apm,
    pcm20: Vec<f32>,
    pcm20_len: usize,
    send: Vec<f32>,
    pkt: Vec<u8>,
    seq: u16,
    ts: u32,
    talkspurt: bool,
    prev_sent: bool,
    vad: LocalVad,
    local_speaking: bool,
    dtx: bool,
    fec: bool,
    loss_pct: u8,
    sent: u64,
    dropped: u64,
}

struct RemoteTrack {
    id: u16,
    jb: JitterBuffer,
    dec: Box<dyn OpusDecode>,
    frame: Vec<f32>,
    cursor: usize,
    silent: bool,
    speaking: RemoteSpeaking,
}

impl RemoteTrack {
    /// Produces the next 20 ms of this track into `self.frame`.
    fn next_frame(&mut self, now_ms: u64) {
        self.cursor = 0;
        match self.jb.pull(now_ms) {
            Playout::Silence => self.silence(),
            Playout::Decode(p) => {
                let r = self.dec.decode(&p, &mut self.frame);
                self.decoded(r);
            }
            Playout::Fec(p) => {
                let r = self.dec.decode_fec(&p, &mut self.frame);
                self.decoded(r);
            }
            Playout::Conceal => {
                // concealment right after silence would only extrapolate old state
                if self.silent || self.dec.conceal(&mut self.frame).is_err() {
                    self.silence();
                }
            }
            Playout::ConcealFadeOut => {
                if !self.silent && self.dec.conceal(&mut self.frame).is_ok() {
                    fade_out(&mut self.frame);
                } else {
                    self.frame.fill(0.0);
                }
                self.silent = true;
            }
        }
    }

    fn decoded(&mut self, r: Result<usize>) {
        match r {
            Ok(n) => {
                if n < FRAME_SAMPLES {
                    self.frame[n..].fill(0.0);
                }
                if self.silent {
                    fade_in(&mut self.frame, FADE_IN);
                }
                self.silent = false;
            }
            // undecodable packet: conceal instead
            Err(_) => {
                if self.silent || self.dec.conceal(&mut self.frame).is_err() {
                    self.silence();
                }
            }
        }
    }

    fn silence(&mut self) {
        self.frame.fill(0.0);
        self.silent = true;
    }
}

struct Voice {
    clip: Arc<ClipPcm>,
    play_pos: usize,
    send_pos: usize,
    soundboard: bool,
}

/// See the module docs.
pub(crate) struct Processor {
    codec: Arc<dyn OpusCodec>,
    events: mpsc::UnboundedSender<AudioEvent>,
    settings: Settings,
    muted: bool,
    deafened: bool,
    call: Option<CallState>,
    tracks: Vec<RemoteTrack>,
    gains: HashMap<u16, f32>,
    voices: Vec<Voice>,
    mic_test: bool,
    meter_sum: f32,
    meter_frames: u32,
    loss_pct: u8,
    aec_delay_ms: i32,
}

/// Bitrate for a reported loss: 24 kbps above 10 % loss, else 32 kbps.
pub(crate) fn bitrate_for_loss(loss_pct: u8) -> i32 {
    if loss_pct > 10 { 24_000 } else { 32_000 }
}

/// Opus packet-loss-perc for a reported loss, clamped to 5..=20.
pub(crate) fn loss_perc_for(loss_pct: u8) -> i32 {
    i32::from(loss_pct.clamp(5, 20))
}

impl Processor {
    pub(crate) fn new(codec: Arc<dyn OpusCodec>, events: mpsc::UnboundedSender<AudioEvent>, settings: Settings) -> Self {
        Self {
            codec,
            events,
            settings,
            muted: false,
            deafened: false,
            call: None,
            tracks: Vec::new(),
            gains: HashMap::new(),
            voices: Vec::new(),
            mic_test: false,
            meter_sum: 0.0,
            meter_frames: 0,
            loss_pct: 10,
            aec_delay_ms: 0,
        }
    }

    fn emit(&self, ev: AudioEvent) {
        let _ = self.events.send(ev);
    }

    // ------------------------------------------------------------------ call control

    /// Starts sending with `enc` (created by the caller so codec errors surface synchronously).
    /// A running call is stopped first.
    pub(crate) fn start_call(&mut self, track: u16, epoch: u8, out: mpsc::Sender<Bytes>, mut enc: Box<dyn OpusEncode>) {
        if self.call.is_some() {
            self.stop_call();
        }
        let loss = self.loss_pct;
        for r in [enc.set_packet_loss_perc(loss_perc_for(loss)), enc.set_bitrate(bitrate_for_loss(loss)), enc.set_dtx(true)] {
            if let Err(e) = r {
                tracing::warn!(%e, "opus encoder setup");
            }
        }
        self.call = Some(CallState {
            track,
            epoch,
            out,
            enc,
            apm: Apm::new(self.settings.echo_cancellation, self.settings.noise_suppression),
            pcm20: vec![0.0; FRAME_SAMPLES],
            pcm20_len: 0,
            send: vec![0.0; FRAME_SAMPLES],
            pkt: vec![0; MAX_OPUS_PACKET],
            seq: rand::random(),
            ts: rand::random(),
            talkspurt: true,
            prev_sent: false,
            vad: LocalVad::default(),
            local_speaking: false,
            dtx: true,
            fec: true,
            loss_pct: loss,
            sent: 0,
            dropped: 0,
        });
        tracing::info!(track, epoch, "call started");
    }

    /// Stops sending, drops every remote track and cancels soundboard clips.
    pub(crate) fn stop_call(&mut self) {
        if let Some(call) = self.call.take() {
            tracing::info!(sent = call.sent, dropped = call.dropped, "call stopped");
            if call.local_speaking {
                self.emit(AudioEvent::LocalSpeaking(false));
            }
        }
        for t in std::mem::take(&mut self.tracks) {
            if t.speaking.speaking() {
                self.emit(AudioEvent::Speaking { track: t.id, speaking: false });
            }
        }
        self.cancel_soundboard();
    }

    pub(crate) fn in_call(&self) -> bool {
        self.call.is_some()
    }

    pub(crate) fn set_epoch(&mut self, epoch: u8) {
        if let Some(c) = &mut self.call {
            c.epoch = epoch;
        }
    }

    pub(crate) fn remove_track(&mut self, track: u16) {
        if let Some(i) = self.tracks.iter().position(|t| t.id == track) {
            let t = self.tracks.swap_remove(i);
            if t.speaking.speaking() {
                self.emit(AudioEvent::Speaking { track, speaking: false });
            }
        }
    }

    pub(crate) fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    pub(crate) fn set_deafened(&mut self, deafened: bool) {
        self.deafened = deafened;
        if deafened {
            self.cancel_soundboard();
        }
    }

    pub(crate) fn set_track_volume(&mut self, track: u16, gain: f32) {
        let g = if gain.is_finite() { gain.clamp(0.0, 4.0) } else { 1.0 };
        self.gains.insert(track, g);
    }

    pub(crate) fn set_input_gain(&mut self, g: f32) {
        self.settings.input_gain = if g.is_finite() { g.clamp(0.0, 4.0) } else { 1.0 };
    }

    pub(crate) fn set_output_gain(&mut self, g: f32) {
        self.settings.output_gain = if g.is_finite() { g.clamp(0.0, 4.0) } else { 1.0 };
    }

    pub(crate) fn set_processing(&mut self, echo_cancellation: bool, noise_suppression: bool) {
        self.settings.echo_cancellation = echo_cancellation;
        self.settings.noise_suppression = noise_suppression;
        if let Some(c) = &mut self.call {
            c.apm.set_processing(echo_cancellation, noise_suppression);
        }
    }

    pub(crate) fn set_loss_hint(&mut self, loss_pct: u8) {
        self.loss_pct = loss_pct.min(100);
        if let Some(c) = &mut self.call
            && c.loss_pct != self.loss_pct
        {
            c.loss_pct = self.loss_pct;
            if let Err(e) = c.enc.set_packet_loss_perc(loss_perc_for(self.loss_pct)) {
                tracing::warn!(%e, "opus loss perc");
            }
            if let Err(e) = c.enc.set_bitrate(bitrate_for_loss(self.loss_pct)) {
                tracing::warn!(%e, "opus bitrate");
            }
        }
    }

    pub(crate) fn set_jitter_config(&mut self, cfg: JitterConfig) {
        self.settings.jitter = cfg;
        for t in &mut self.tracks {
            t.jb.set_config(cfg);
        }
    }

    pub(crate) fn set_aec_delay_ms(&mut self, ms: i32) {
        self.aec_delay_ms = ms;
    }

    pub(crate) fn set_mic_test(&mut self, on: bool) {
        self.mic_test = on;
        self.meter_sum = 0.0;
        self.meter_frames = 0;
    }

    pub(crate) fn mic_test(&self) -> bool {
        self.mic_test
    }

    // ------------------------------------------------------------------ clips

    /// Plays a soundboard clip into the call and locally. Allowed while muted.
    pub(crate) fn play_clip(&mut self, clip: Arc<ClipPcm>) -> Result<()> {
        if self.call.is_none() {
            return Err(AudioError::NotInCall);
        }
        if self.deafened {
            return Err(AudioError::Deafened);
        }
        self.add_voice(Voice { play_pos: 0, send_pos: 0, soundboard: true, clip });
        Ok(())
    }

    /// Plays a UI sound locally only (also while deafened).
    pub(crate) fn play_local(&mut self, clip: Arc<ClipPcm>) {
        let len = clip.len();
        self.add_voice(Voice { play_pos: 0, send_pos: len, soundboard: false, clip });
    }

    fn add_voice(&mut self, v: Voice) {
        if v.clip.is_empty() {
            if v.soundboard {
                self.emit(AudioEvent::ClipFinished);
            }
            return;
        }
        if self.voices.len() >= MAX_VOICES {
            let old = self.voices.remove(0);
            if old.soundboard {
                self.emit(AudioEvent::ClipFinished);
            }
        }
        self.voices.push(v);
    }

    fn cancel_soundboard(&mut self) {
        let before = self.voices.len();
        self.voices.retain(|v| !v.soundboard);
        for _ in self.voices.len()..before {
            self.emit(AudioEvent::ClipFinished);
        }
    }

    fn reap_voices(&mut self) {
        let mut finished = 0;
        self.voices.retain(|v| {
            let done = v.play_pos >= v.clip.len() && v.send_pos >= v.clip.len();
            if done && v.soundboard {
                finished += 1;
            }
            !done
        });
        for _ in 0..finished {
            self.emit(AudioEvent::ClipFinished);
        }
    }

    /// True while a clip still has samples to play locally (keeps the output device open).
    pub(crate) fn has_local_voices(&self) -> bool {
        self.voices.iter().any(|v| v.play_pos < v.clip.len())
    }

    // ------------------------------------------------------------------ receive

    /// Routes one received audio datagram to its track's jitter buffer.
    pub(crate) fn push_packet(&mut self, pkt: Bytes, now_ms: u64) {
        let Some(call) = &self.call else { return };
        let own = call.track;
        let Some((h, opus)) = AudioHeader::parse(&pkt) else { return };
        if h.track == own || opus.is_empty() || opus.len() > MAX_OPUS_PACKET {
            return;
        }
        let idx = match self.tracks.iter().position(|t| t.id == h.track) {
            Some(i) => i,
            None => {
                if self.tracks.len() >= MAX_TRACKS {
                    return;
                }
                let dec = match self.codec.decoder() {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(%e, "opus decoder");
                        return;
                    }
                };
                self.tracks.push(RemoteTrack {
                    id: h.track,
                    jb: JitterBuffer::new(self.settings.jitter),
                    dec,
                    frame: vec![0.0; FRAME_SAMPLES],
                    cursor: FRAME_SAMPLES,
                    silent: true,
                    speaking: RemoteSpeaking::default(),
                });
                self.tracks.len() - 1
            }
        };
        let t = &mut self.tracks[idx];
        let change = t.speaking.on_packet(h.flags & AF_SPEAKING != 0, now_ms);
        t.jb.push(&h, pkt.slice(AUDIO_HEADER_LEN..), now_ms);
        if let Some(speaking) = change {
            self.emit(AudioEvent::Speaking { track: h.track, speaking });
        }
    }

    /// Expires remote speaking holds. Call at least every few tens of milliseconds in a call.
    pub(crate) fn tick(&mut self, now_ms: u64) {
        let mut changes = [(0u16, false); MAX_TRACKS];
        let mut n = 0;
        for t in &mut self.tracks {
            if let Some(s) = t.speaking.on_tick(now_ms) {
                changes[n] = (t.id, s);
                n += 1;
            }
        }
        for &(track, speaking) in &changes[..n] {
            self.emit(AudioEvent::Speaking { track, speaking });
        }
    }

    // ------------------------------------------------------------------ playout

    /// Mixes the next 10 ms for the speaker into `out` (480 samples). When `to_speaker` is true
    /// the result is also the echo canceller's render reference.
    pub(crate) fn render(&mut self, out: &mut [f32], now_ms: u64, to_speaker: bool) {
        out.fill(0.0);
        let deaf = self.deafened;
        let n = out.len().min(FRAME_10MS);
        for t in &mut self.tracks {
            if t.cursor + n > FRAME_SAMPLES {
                t.next_frame(now_ms);
            }
            let gain = self.gains.get(&t.id).copied().unwrap_or(1.0);
            if !deaf && gain > 0.0 {
                for (o, s) in out.iter_mut().zip(&t.frame[t.cursor..t.cursor + n]) {
                    *o += s * gain;
                }
            }
            t.cursor += n;
        }
        for v in &mut self.voices {
            let len = v.clip.len();
            if v.play_pos >= len {
                continue;
            }
            let end = (v.play_pos + n).min(len);
            let gain = if v.soundboard { SOUNDBOARD_GAIN } else { 1.0 };
            for (o, s) in out.iter_mut().zip(&v.clip.samples()[v.play_pos..end]) {
                *o += s * gain;
            }
            v.play_pos = end;
        }
        self.reap_voices();
        let g = self.settings.output_gain;
        for o in out.iter_mut() {
            *o = soft_limit(*o * g);
        }
        if to_speaker && let Some(c) = &mut self.call {
            c.apm.render(out);
        }
    }

    // ------------------------------------------------------------------ capture

    /// Processes 10 ms of microphone audio (480 samples, modified in place).
    pub(crate) fn capture(&mut self, frame: &mut [f32], _now_ms: u64) {
        let g = self.settings.input_gain;
        if g != 1.0 {
            for s in frame.iter_mut() {
                *s *= g;
            }
        }
        if self.mic_test {
            let r = rms(frame);
            self.meter_sum += r * r;
            self.meter_frames += 1;
            if self.meter_frames >= METER_FRAMES {
                let level = meter_level((self.meter_sum / self.meter_frames as f32).sqrt());
                self.meter_sum = 0.0;
                self.meter_frames = 0;
                self.emit(AudioEvent::MicLevel(level));
            }
        }
        let delay = self.aec_delay_ms;
        let Some(call) = &mut self.call else { return };
        if frame.len() != FRAME_10MS {
            return;
        }
        call.apm.set_delay_ms(delay);
        call.apm.capture(frame);
        call.pcm20[call.pcm20_len..call.pcm20_len + FRAME_10MS].copy_from_slice(frame);
        call.pcm20_len += FRAME_10MS;
        if call.pcm20_len >= FRAME_SAMPLES {
            call.pcm20_len = 0;
            self.encode_frame();
        }
    }

    fn encode_frame(&mut self) {
        let muted = self.muted || self.deafened;
        let Some(call) = self.call.as_mut() else { return };
        let voice_db = dbov(rms(&call.pcm20));
        let mut speaking = call.vad.update(voice_db, FRAME_MS);
        if muted {
            speaking = false;
            call.vad.reset();
            call.send.fill(0.0);
        } else {
            call.send.copy_from_slice(&call.pcm20);
        }
        let mut clip_active = false;
        for v in self.voices.iter_mut().filter(|v| v.soundboard) {
            let len = v.clip.len();
            if v.send_pos >= len {
                continue;
            }
            clip_active = true;
            let end = (v.send_pos + FRAME_SAMPLES).min(len);
            for (o, s) in call.send.iter_mut().zip(&v.clip.samples()[v.send_pos..end]) {
                *o += s * SOUNDBOARD_GAIN;
            }
            v.send_pos = end;
        }
        if clip_active {
            soft_limit_all(&mut call.send);
        }
        let speaking_changed = speaking != call.local_speaking;
        call.local_speaking = speaking;
        let ts = call.ts;
        call.ts = call.ts.wrapping_add(FRAME_SAMPLES as u32);
        self.send_frame(muted, clip_active, speaking, ts);
        if speaking_changed {
            self.emit(AudioEvent::LocalSpeaking(speaking));
        }
        self.reap_voices();
    }

    fn send_frame(&mut self, muted: bool, clip_active: bool, speaking: bool, ts: u32) {
        let Some(call) = self.call.as_mut() else { return };
        if muted && !clip_active {
            // muted: send nothing, the sequence number is not consumed (like DTX)
            call.talkspurt = true;
            call.prev_sent = false;
            return;
        }
        let want_dtx = !clip_active;
        if want_dtx != call.dtx {
            if let Err(e) = call.enc.set_dtx(want_dtx) {
                tracing::warn!(%e, "opus dtx");
            }
            call.dtx = want_dtx;
        }
        let n = match call.enc.encode(&call.send, &mut call.pkt) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(%e, "opus encode");
                return;
            }
        };
        if n <= 2 {
            // DTX frame: not transmitted, sequence number not consumed
            call.talkspurt = true;
            call.prev_sent = false;
            return;
        }
        let opus = &call.pkt[..n];
        let mut flags = 0u8;
        if speaking {
            flags |= AF_SPEAKING;
        }
        if call.talkspurt {
            flags |= AF_TALKSPURT;
        }
        if call.fec && call.prev_sent && packet_has_silk(opus) {
            flags |= AF_FEC;
        }
        if clip_active {
            flags |= AF_SOUNDBOARD;
        }
        let header = AudioHeader { flags, track: call.track, seq: call.seq, ts, level: level_byte(rms(&call.send)), epoch: call.epoch };
        match call.out.try_send(header.packet(opus)) {
            Ok(()) => call.sent += 1,
            Err(_) => call.dropped += 1,
        }
        call.seq = call.seq.wrapping_add(1);
        call.talkspurt = false;
        call.prev_sent = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::LibOpus;
    use crate::codec::EncoderSettings;
    use tincan_core::media::AudioHeader;

    fn settings() -> Settings {
        Settings {
            echo_cancellation: true,
            noise_suppression: true,
            input_gain: 1.0,
            output_gain: 1.0,
            jitter: JitterConfig::default(),
        }
    }

    fn processor() -> (Processor, mpsc::UnboundedReceiver<AudioEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Processor::new(Arc::new(LibOpus), tx, settings()), rx)
    }

    fn start(p: &mut Processor, track: u16) -> mpsc::Receiver<Bytes> {
        let (tx, rx) = mpsc::channel(10_000);
        let enc = LibOpus.encoder(&EncoderSettings::default()).unwrap();
        p.start_call(track, 7, tx, enc);
        rx
    }

    /// Speech-like test signal: pitch-modulated harmonics with a syllable envelope.
    fn speech(i: usize) -> f32 {
        let t = i as f32 / 48_000.0;
        // f0 = 150 Hz +- 25 Hz at 2.5 Hz; phase is its integral
        let phase = 2.0 * std::f32::consts::PI * (150.0 * t - 25.0 / (2.0 * std::f32::consts::PI * 2.5) * (2.0 * std::f32::consts::PI * 2.5 * t).cos());
        let env = 0.6 + 0.4 * (2.0 * std::f32::consts::PI * 4.0 * t).sin();
        0.08 * env * ((phase).sin() + 0.5 * (2.0 * phase).sin() + 0.25 * (3.0 * phase).sin())
    }

    fn drain(rx: &mut mpsc::Receiver<Bytes>) -> Vec<Bytes> {
        let mut v = Vec::new();
        while let Ok(p) = rx.try_recv() {
            v.push(p);
        }
        v
    }

    fn events(rx: &mut mpsc::UnboundedReceiver<AudioEvent>) -> Vec<AudioEvent> {
        let mut v = Vec::new();
        while let Ok(e) = rx.try_recv() {
            v.push(e);
        }
        v
    }

    /// Runs `frames` 10 ms ticks: render (discarded) then capture from `src(sample_index)`.
    fn run(p: &mut Processor, start_frame: usize, frames: usize, src: impl Fn(usize) -> f32) {
        let mut buf = vec![0f32; FRAME_10MS];
        let mut out = vec![0f32; FRAME_10MS];
        for f in start_frame..start_frame + frames {
            let now = f as u64 * 10;
            p.render(&mut out, now, true);
            for (i, s) in buf.iter_mut().enumerate() {
                *s = src(f * FRAME_10MS + i);
            }
            p.capture(&mut buf, now);
            p.tick(now);
        }
    }

    #[test]
    fn dtx_silence_sends_few_packets_and_seq_stays_contiguous() {
        let (mut p, _ev) = processor();
        let mut rx = start(&mut p, 4);
        run(&mut p, 0, 300, |_| 0.0); // 3 s of silence
        let pkts = drain(&mut rx);
        assert!(!pkts.is_empty() && pkts.len() < 30, "{} packets for 3 s of silence", pkts.len());
        let hs: Vec<AudioHeader> = pkts.iter().map(|b| AudioHeader::parse(b).unwrap().0).collect();
        assert!(hs.windows(2).all(|w| w[1].seq == w[0].seq.wrapping_add(1)), "DTX must not consume seq");
        assert!(hs.windows(2).any(|w| w[1].ts.wrapping_sub(w[0].ts) > 960), "ts jumps over DTX");
        assert!(hs.iter().all(|h| h.flags & AF_SPEAKING == 0));
        // speech afterwards: every frame is sent, the first one flagged as talkspurt start
        run(&mut p, 300, 100, speech);
        let hs: Vec<AudioHeader> = drain(&mut rx).iter().map(|b| AudioHeader::parse(b).unwrap().0).collect();
        assert!(hs.len() >= 48, "{} packets for 1 s of speech", hs.len());
        assert!(hs[0].flags & AF_TALKSPURT != 0);
        assert!(hs.iter().any(|h| h.flags & AF_SPEAKING != 0));
        // 32 kbps VOIP voice is SILK or hybrid, so in-band FEC is actually carried
        let fec = hs.iter().filter(|h| h.flags & AF_FEC != 0).count();
        assert!(fec * 10 >= hs.len() * 8, "{fec} of {} packets flagged FEC", hs.len());
        assert!(hs.iter().all(|h| h.track == 4 && h.epoch == 7));
    }

    #[test]
    fn local_speaking_toggles_with_hangover() {
        let (mut p, mut ev) = processor();
        let _rx = start(&mut p, 4);
        run(&mut p, 0, 100, speech);
        run(&mut p, 100, 100, |_| 0.0);
        run(&mut p, 200, 100, speech);
        let local: Vec<bool> = events(&mut ev)
            .into_iter()
            .filter_map(|e| if let AudioEvent::LocalSpeaking(s) = e { Some(s) } else { None })
            .collect();
        assert_eq!(local, vec![true, false, true]);
        p.set_muted(true);
        run(&mut p, 300, 10, speech);
        assert!(events(&mut ev).contains(&AudioEvent::LocalSpeaking(false)), "mute clears local speaking");
    }

    #[test]
    fn muted_sends_nothing() {
        let (mut p, _ev) = processor();
        let mut rx = start(&mut p, 4);
        p.set_muted(true);
        run(&mut p, 0, 100, speech);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn soundboard_while_muted_is_flagged_and_loud_and_dtx_off() {
        let (mut p, mut ev) = processor();
        let mut rx = start(&mut p, 8);
        p.set_muted(true);
        let clip: Vec<f32> = (0..48_000).map(|i| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin()).collect();
        p.play_clip(Arc::new(ClipPcm::from_samples(clip))).unwrap();
        run(&mut p, 0, 120, |_| 0.0);
        let pkts = drain(&mut rx);
        assert_eq!(pkts.len(), 50, "1 s clip = 50 packets, DTX off, voice muted");
        let mut dec = LibOpus.decoder().unwrap();
        let mut pcm = vec![0f32; 960];
        let mut energy = 0.0;
        for b in &pkts {
            let (h, opus) = AudioHeader::parse(b).unwrap();
            assert!(h.flags & AF_SOUNDBOARD != 0);
            assert!(h.flags & AF_SPEAKING == 0, "voice is muted");
            assert!(h.level < 20, "level byte {}", h.level);
            dec.decode(opus, &mut pcm).unwrap();
            energy += pcm.iter().map(|x| x * x).sum::<f32>();
        }
        assert!(energy / (50.0 * 960.0) > 0.01, "clip carries energy");
        assert!(events(&mut ev).contains(&AudioEvent::ClipFinished));
        // after the clip: muted again, nothing sent
        run(&mut p, 120, 50, |_| 0.0);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn play_clip_errors() {
        let (mut p, _ev) = processor();
        let clip = Arc::new(ClipPcm::from_samples(vec![0.1; 4800]));
        assert!(matches!(p.play_clip(clip.clone()), Err(AudioError::NotInCall)));
        let _rx = start(&mut p, 4);
        p.set_deafened(true);
        assert!(matches!(p.play_clip(clip.clone()), Err(AudioError::Deafened)));
        p.set_deafened(false);
        assert!(p.play_clip(clip).is_ok());
    }

    #[test]
    fn two_processors_carry_audio_and_speaking() {
        let (mut a, _eva) = processor();
        let (mut b, mut evb) = processor();
        let mut rx_a = start(&mut a, 4);
        let _rx_b = start(&mut b, 8);
        let mut cap = vec![0f32; FRAME_10MS];
        let mut out = vec![0f32; FRAME_10MS];
        let mut received_energy = 0.0f32;
        for f in 0..300usize {
            let now = f as u64 * 10;
            a.render(&mut out, now, true);
            for (i, s) in cap.iter_mut().enumerate() {
                let n = f * FRAME_10MS + i;
                *s = if (100..200).contains(&f) { 0.0 } else { speech(n) };
            }
            a.capture(&mut cap, now);
            for pkt in drain(&mut rx_a) {
                b.push_packet(pkt, now + 30);
            }
            b.render(&mut out, now + 30, true);
            if f > 20 && f < 100 {
                received_energy += out.iter().map(|x| x * x).sum::<f32>();
            }
            cap.fill(0.0);
            b.capture(&mut cap, now + 30);
            b.tick(now + 30);
        }
        assert!(received_energy > 1.0, "energy {received_energy}");
        let sp: Vec<AudioEvent> = events(&mut evb).into_iter().filter(|e| matches!(e, AudioEvent::Speaking { .. })).collect();
        assert_eq!(
            sp,
            vec![
                AudioEvent::Speaking { track: 4, speaking: true },
                AudioEvent::Speaking { track: 4, speaking: false },
                AudioEvent::Speaking { track: 4, speaking: true },
            ]
        );
        b.remove_track(4);
        assert_eq!(events(&mut evb), vec![AudioEvent::Speaking { track: 4, speaking: false }]);
    }

    #[test]
    fn loss_hint_maps_to_bitrate_and_perc() {
        assert_eq!(loss_perc_for(0), 5);
        assert_eq!(loss_perc_for(50), 20);
        assert_eq!(bitrate_for_loss(10), 32_000);
        assert_eq!(bitrate_for_loss(11), 24_000);
    }
}
