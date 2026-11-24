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
use crate::denoise::Denoiser;
use crate::codec::{EncoderSettings, MAX_OPUS_PACKET, OpusCodec, OpusDecode, OpusEncode, packet_has_silk};
use crate::dsp::{LocalVad, RemoteSpeaking, dbov, fade_in, fade_out, level_byte, meter_level, rms, soft_limit, soft_limit_all};
use crate::error::{AudioError, Result};
use crate::jitter::{FRAME_MS, FRAME_SAMPLES, JitterBuffer, JitterConfig, Playout};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;
use tincan_core::media::{
    AF_FEC, AF_SOUNDBOARD, AF_SPEAKING, AF_TALKSPURT, AUDIO_HEADER_LEN, AudioHeader, KIND_SHARE_AUDIO, KIND_SOUND, is_audio_kind, track_id, track_kind,
    track_short_id,
};
use tokio::sync::mpsc;

/// Samples per processing frame (10 ms at 48 kHz).
pub(crate) const FRAME_10MS: usize = APM_FRAME;
/// Remote tracks kept at once (a voice and a soundboard track per person); packets for
/// further track ids are ignored.
pub(crate) const MAX_TRACKS: usize = 64;
/// Screen share audio streams kept at once (separate from `MAX_TRACKS`); packets for
/// further share-audio tracks are ignored.
pub(crate) const MAX_SHARE_STREAMS: usize = 4;
/// Clip voices playing at once; the oldest is cut when another starts.
pub(crate) const MAX_VOICES: usize = 8;
/// Gain applied to soundboard clips (in the call and locally), -3 dB.
pub(crate) const SOUNDBOARD_GAIN: f32 = 0.7;
/// Fade-out of a clip that is stopped (5 ms, like the clip edges).
pub(crate) const STOP_RAMP: usize = 240;
/// Bitrate of the soundboard stream (clips are often music, so a little above voice).
const SOUND_BITRATE: i32 = 40_000;
/// Fade-in length after silence or concealment, to avoid clicks (10 ms).
const FADE_IN: usize = 480;
/// Capture frames per `MicLevel` event (50 ms -> 20 events per second).
const METER_FRAMES: u32 = 5;

/// Runtime settings of the processor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Settings {
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    /// Noise suppression by DPDFNet instead of WebRTC's suppressor (when the model is there).
    pub ai_noise_suppression: bool,
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
    /// DPDFNet, after the APM; None runs WebRTC noise suppression in the APM instead.
    denoise: Option<Denoiser>,
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
    fec: bool,
    /// The soundboard stream (`KIND_SOUND` under my short id); None if its encoder failed.
    sound: Option<SoundStream>,
    loss_pct: u8,
    sent: u64,
    dropped: u64,
}

/// My clips go out on their own Opus stream, so each receiver can set its volume apart from my
/// voice. It shares the voice's sample clock, so a pause between clips reads as a pause (seq
/// contiguous, ts jump) at the receiver's jitter buffer. Nothing is sent while no clip plays.
struct SoundStream {
    enc: Box<dyn OpusEncode>,
    pcm: Vec<f32>,
    pkt: Vec<u8>,
    seq: u16,
    talkspurt: bool,
    prev_sent: bool,
}

struct RemoteTrack {
    id: u16,
    jb: JitterBuffer,
    dec: Box<dyn OpusDecode>,
    frame: Vec<f32>,
    cursor: usize,
    silent: bool,
    /// Peak of the last decoded frame, the yardstick for concealment output.
    last_peak: f32,
    speaking: RemoteSpeaking,
}

/// Concealment may continue the last frame, never exceed it by much. unsafe-libopus 0.2 PLC
/// can emit a near full-scale burst after low-level hybrid frames (room noise between words)
/// and leave the decoder in that state: heard as a loud click on every late packet.
fn concealment_is_sane(out: &[f32], last_peak: f32) -> bool {
    peak(out) <= 2.0 * last_peak + 0.02
}

fn peak(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, v| m.max(v.abs()))
}

impl RemoteTrack {
    /// Produces the next 20 ms of this track into `self.frame`.
    fn next_frame(&mut self, now_ms: u64) {
        self.cursor = 0;
        match self.jb.pull(now_ms) {
            // A pause right after audio (the end of a talkspurt, or a lone DTX comfort-noise
            // refresh frame): fade the decoder's continuation out instead of jumping to zero,
            // which clicks.
            Playout::Silence => self.fade_to_silence(),
            Playout::Decode(p) => {
                let r = self.dec.decode(&p, &mut self.frame);
                self.decoded(r);
            }
            Playout::Fec(p) => {
                // FEC falls back to PLC when the packet carries none: same sanity check, with
                // room for a louder recovered frame
                match self.dec.decode_fec(&p, &mut self.frame) {
                    Ok(_) if peak(&self.frame) > 4.0 * self.last_peak + 0.1 => self.discard(),
                    r => self.decoded(r),
                }
            }
            Playout::Conceal => {
                // concealment right after silence would only extrapolate old state
                if self.silent || !self.conceal() {
                    self.silence();
                }
            }
            Playout::ConcealFadeOut => self.fade_to_silence(),
        }
    }

    /// Ends audible output with one concealed frame faded to zero; zeros when already silent.
    fn fade_to_silence(&mut self) {
        if !self.silent && self.conceal() {
            fade_out(&mut self.frame);
        } else {
            self.frame.fill(0.0);
        }
        self.silent = true;
    }

    fn decoded(&mut self, r: Result<usize>) {
        match r {
            Ok(n) => {
                if n < FRAME_SAMPLES {
                    self.frame[n..].fill(0.0);
                }
                self.last_peak = peak(&self.frame);
                if self.silent {
                    fade_in(&mut self.frame, FADE_IN);
                }
                self.silent = false;
            }
            // undecodable packet: conceal instead
            Err(_) => {
                if self.silent || !self.conceal() {
                    self.silence();
                }
            }
        }
    }

    /// Opus PLC into `self.frame`; false (decoder reset, frame zeroed) when it failed or
    /// produced a burst instead of a continuation.
    fn conceal(&mut self) -> bool {
        match self.dec.conceal(&mut self.frame) {
            Ok(_) if concealment_is_sane(&self.frame, self.last_peak) => true,
            _ => {
                self.discard();
                false
            }
        }
    }

    /// Drops a bad frame: zeros, a fresh decoder state, and a fade-in on the next packet.
    fn discard(&mut self) {
        self.dec.reset();
        self.last_peak = 0.0;
        self.silence();
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
    /// The caller's id of a soundboard clip (`ClipFinished` carries it; 0 for UI sounds).
    id: u64,
    /// Stopped: local playout and sending each fade out over [`STOP_RAMP`] samples from
    /// these positions.
    stop: Option<(usize, usize)>,
}

/// Gain of sample `i` of a voice stopped at `at` (None: not stopped).
fn stop_gain(i: usize, at: Option<usize>) -> f32 {
    match at {
        Some(at) if i >= at => (at + STOP_RAMP).saturating_sub(i) as f32 / STOP_RAMP as f32,
        _ => 1.0,
    }
}

impl Voice {
    /// Where local playout ends.
    fn play_end(&self) -> usize { self.stop.map_or(self.clip.len(), |(p, _)| (p + STOP_RAMP).min(self.clip.len())) }
    /// Where sending ends.
    fn send_end(&self) -> usize { self.stop.map_or(self.clip.len(), |(_, s)| (s + STOP_RAMP).min(self.clip.len())) }
    fn done(&self) -> bool { self.play_pos >= self.play_end() && self.send_pos >= self.send_end() }
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
    /// Screen share audio streams (`KIND_SHARE_AUDIO` tracks), see `push_share`.
    share: Vec<RemoteTrack>,
    gains: HashMap<u16, f32>,
    voices: Vec<Voice>,
    /// Local gain of my own soundboard clips (the room's soundboard volume; not sent).
    local_sound_gain: f32,
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
            share: Vec::new(),
            gains: HashMap::new(),
            voices: Vec::new(),
            local_sound_gain: 1.0,
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
        let sound = match self.codec.encoder(&EncoderSettings { bitrate_bps: SOUND_BITRATE, dtx: false, ..EncoderSettings::default() }) {
            Ok(mut enc) => {
                if let Err(e) = enc.set_packet_loss_perc(loss_perc_for(loss)) {
                    tracing::warn!(%e, "opus encoder setup (soundboard)");
                }
                Some(SoundStream { enc, pcm: vec![0.0; FRAME_SAMPLES], pkt: vec![0; MAX_OPUS_PACKET], seq: rand::random(), talkspurt: true, prev_sent: false })
            }
            Err(e) => {
                tracing::warn!(%e, "soundboard encoder; clips will not be sent");
                None
            }
        };
        let denoise = self.wants_ai_denoise().then(Denoiser::new).flatten();
        self.call = Some(CallState {
            track,
            epoch,
            out,
            enc,
            apm: Apm::new(self.settings.echo_cancellation, self.settings.noise_suppression && denoise.is_none()),
            denoise,
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
            fec: true,
            sound,
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
        self.share.clear();
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
        self.share.retain(|t| t.id != track);
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

    fn wants_ai_denoise(&self) -> bool {
        self.settings.noise_suppression && self.settings.ai_noise_suppression
    }

    pub(crate) fn set_processing(&mut self, echo_cancellation: bool, noise_suppression: bool, ai_noise_suppression: bool) {
        self.settings.echo_cancellation = echo_cancellation;
        self.settings.noise_suppression = noise_suppression;
        self.settings.ai_noise_suppression = ai_noise_suppression;
        let ai = self.wants_ai_denoise();
        if let Some(c) = &mut self.call {
            // keep a running denoiser's state; start a fresh one when it is switched on
            if !ai {
                c.denoise = None;
            } else if c.denoise.is_none() {
                c.denoise = Denoiser::new();
            }
            c.apm.set_processing(echo_cancellation, noise_suppression && c.denoise.is_none());
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
            if let Some(s) = &mut c.sound
                && let Err(e) = s.enc.set_packet_loss_perc(loss_perc_for(self.loss_pct))
            {
                tracing::warn!(%e, "opus loss perc (soundboard)");
            }
        }
    }

    pub(crate) fn set_jitter_config(&mut self, cfg: JitterConfig) {
        self.settings.jitter = cfg;
        for t in self.tracks.iter_mut().chain(self.share.iter_mut()) {
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

    /// Plays soundboard clip `id` into the call (its own stream) and locally. Allowed while
    /// muted. `ClipFinished(id)` follows when it has been sent and played, was stopped or
    /// cut, and also right away when it could not start (the error is returned too).
    pub(crate) fn play_clip(&mut self, clip: Arc<ClipPcm>, id: u64) -> Result<()> {
        let refused = if self.call.is_none() {
            Some(AudioError::NotInCall)
        } else if self.deafened {
            Some(AudioError::Deafened)
        } else {
            None
        };
        if let Some(e) = refused {
            self.emit(AudioEvent::ClipFinished(id));
            return Err(e);
        }
        self.add_voice(Voice { play_pos: 0, send_pos: 0, soundboard: true, clip, id, stop: None });
        Ok(())
    }

    /// Plays a UI sound locally only (also while deafened).
    pub(crate) fn play_local(&mut self, clip: Arc<ClipPcm>) {
        let len = clip.len();
        self.add_voice(Voice { play_pos: 0, send_pos: len, soundboard: false, clip, id: 0, stop: None });
    }

    /// Stops soundboard clip `id` here and in the call at once (a 5 ms fade, so it does not
    /// click). `ClipFinished(id)` follows once the fade is out. Unknown ids are ignored.
    pub(crate) fn stop_clip(&mut self, id: u64) {
        for v in self.voices.iter_mut().filter(|v| v.soundboard && v.id == id && v.stop.is_none()) {
            v.stop = Some((v.play_pos, v.send_pos));
        }
    }

    /// Stops every soundboard clip of mine (see [`Processor::stop_clip`]).
    pub(crate) fn stop_all_clips(&mut self) {
        for v in self.voices.iter_mut().filter(|v| v.soundboard && v.stop.is_none()) {
            v.stop = Some((v.play_pos, v.send_pos));
        }
    }

    /// Local gain of my own soundboard clips (0..=4; what is sent is not changed).
    pub(crate) fn set_local_sound_gain(&mut self, g: f32) {
        self.local_sound_gain = if g.is_finite() { g.clamp(0.0, 4.0) } else { 1.0 };
    }

    fn add_voice(&mut self, v: Voice) {
        if v.clip.is_empty() {
            if v.soundboard {
                self.emit(AudioEvent::ClipFinished(v.id));
            }
            return;
        }
        if self.voices.len() >= MAX_VOICES {
            let old = self.voices.remove(0);
            if old.soundboard {
                self.emit(AudioEvent::ClipFinished(old.id));
            }
        }
        self.voices.push(v);
    }

    fn cancel_soundboard(&mut self) {
        let mut cut = Vec::new();
        self.voices.retain(|v| {
            if v.soundboard {
                cut.push(v.id);
            }
            !v.soundboard
        });
        for id in cut {
            self.emit(AudioEvent::ClipFinished(id));
        }
    }

    fn reap_voices(&mut self) {
        let mut finished = Vec::new();
        self.voices.retain(|v| {
            let done = v.done();
            if done && v.soundboard {
                finished.push(v.id);
            }
            !done
        });
        for id in finished {
            self.emit(AudioEvent::ClipFinished(id));
        }
    }

    /// True while a clip still has samples to play locally (keeps the output device open).
    pub(crate) fn has_local_voices(&self) -> bool {
        self.voices.iter().any(|v| v.play_pos < v.play_end())
    }

    // ------------------------------------------------------------------ receive

    /// Routes one received audio datagram to its track's jitter buffer.
    pub(crate) fn push_packet(&mut self, pkt: Bytes, now_ms: u64) {
        let Some(call) = &self.call else { return };
        let own = track_short_id(call.track);
        let Some((h, opus)) = AudioHeader::parse(&pkt) else { return };
        // My own voice, soundboard and share audio never play back.
        if track_short_id(h.track) == own || opus.is_empty() || opus.len() > MAX_OPUS_PACKET {
            return;
        }
        if track_kind(h.track) == KIND_SHARE_AUDIO {
            self.push_share(&h, &pkt, now_ms);
            return;
        }
        if !is_audio_kind(track_kind(h.track)) {
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
                    last_peak: 0.0,
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
        let local_sound = self.local_sound_gain;
        self.mix_share(out, now_ms, n);
        for v in &mut self.voices {
            let len = v.play_end();
            if v.play_pos >= len {
                continue;
            }
            let end = (v.play_pos + n).min(len);
            let gain = if v.soundboard { SOUNDBOARD_GAIN * local_sound } else { 1.0 };
            let stop = v.stop.map(|(p, _)| p);
            for (i, (o, s)) in out.iter_mut().zip(&v.clip.samples()[v.play_pos..end]).enumerate() {
                *o += s * gain * stop_gain(v.play_pos + i, stop);
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
        if let Some(d) = &mut call.denoise {
            d.process(frame);
        }
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
        // My clips, mixed for the soundboard stream (never into the voice).
        let mut clip_active = false;
        if let Some(sound) = call.sound.as_mut() {
            sound.pcm.fill(0.0);
            for v in self.voices.iter_mut().filter(|v| v.soundboard) {
                let len = v.send_end();
                if v.send_pos >= len {
                    continue;
                }
                clip_active = true;
                let end = (v.send_pos + FRAME_SAMPLES).min(len);
                let stop = v.stop.map(|(_, s)| s);
                for (i, (o, s)) in sound.pcm.iter_mut().zip(&v.clip.samples()[v.send_pos..end]).enumerate() {
                    *o += s * SOUNDBOARD_GAIN * stop_gain(v.send_pos + i, stop);
                }
                v.send_pos = end;
            }
            if clip_active {
                soft_limit_all(&mut sound.pcm);
            }
        } else {
            // No soundboard encoder: the clip still plays locally, and is not sent.
            for v in self.voices.iter_mut().filter(|v| v.soundboard) {
                v.send_pos = v.send_end();
            }
        }
        let speaking_changed = speaking != call.local_speaking;
        call.local_speaking = speaking;
        let ts = call.ts;
        call.ts = call.ts.wrapping_add(FRAME_SAMPLES as u32);
        self.send_frame(muted, speaking, ts);
        self.send_sound(clip_active, ts);
        if speaking_changed {
            self.emit(AudioEvent::LocalSpeaking(speaking));
        }
        self.reap_voices();
    }

    /// One 20 ms frame of the soundboard stream, while a clip plays.
    fn send_sound(&mut self, clip_active: bool, ts: u32) {
        let Some(call) = self.call.as_mut() else { return };
        let Some(s) = call.sound.as_mut() else { return };
        if !clip_active {
            // a pause: nothing sent, the sequence number is not consumed
            s.talkspurt = true;
            s.prev_sent = false;
            return;
        }
        let n = match s.enc.encode(&s.pcm, &mut s.pkt) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(%e, "opus encode (soundboard)");
                return;
            }
        };
        let opus = &s.pkt[..n];
        let mut flags = AF_SOUNDBOARD;
        if s.talkspurt {
            flags |= AF_TALKSPURT;
        }
        if call.fec && s.prev_sent && packet_has_silk(opus) {
            flags |= AF_FEC;
        }
        let track = track_id(track_short_id(call.track), KIND_SOUND);
        let header = AudioHeader { flags, track, seq: s.seq, ts, level: level_byte(rms(&s.pcm)), epoch: call.epoch };
        match call.out.try_send(header.packet(opus)) {
            Ok(()) => call.sent += 1,
            Err(_) => call.dropped += 1,
        }
        s.seq = s.seq.wrapping_add(1);
        s.talkspurt = false;
        s.prev_sent = true;
    }

    fn send_frame(&mut self, muted: bool, speaking: bool, ts: u32) {
        let Some(call) = self.call.as_mut() else { return };
        if muted {
            // muted: send nothing, the sequence number is not consumed (like DTX)
            call.talkspurt = true;
            call.prev_sent = false;
            return;
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

// ---------------------------------------------------------------------- screen share audio

impl Processor {
    /// A share-audio datagram (`KIND_SHARE_AUDIO`): into that share's own jitter buffer. Its
    /// Opus is stereo; the mono decoder plays it as a downmix. Share streams never report
    /// speaking (the sharer's ring stays the voice's). Whether to play a share at all is the
    /// caller's decision: the app hands over only the shares being watched.
    fn push_share(&mut self, h: &AudioHeader, pkt: &Bytes, now_ms: u64) {
        let idx = match self.share.iter().position(|t| t.id == h.track) {
            Some(i) => i,
            None => {
                if self.share.len() >= MAX_SHARE_STREAMS {
                    return;
                }
                let dec = match self.codec.decoder() {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(%e, "opus decoder");
                        return;
                    }
                };
                self.share.push(RemoteTrack {
                    id: h.track,
                    jb: JitterBuffer::new(self.settings.jitter),
                    dec,
                    frame: vec![0.0; FRAME_SAMPLES],
                    cursor: FRAME_SAMPLES,
                    silent: true,
                    last_peak: 0.0,
                    speaking: RemoteSpeaking::default(),
                });
                self.share.len() - 1
            }
        };
        self.share[idx].jb.push(h, pkt.slice(AUDIO_HEADER_LEN..), now_ms);
    }

    /// Adds the share streams to the mix (before the output gain and limiter, so the echo
    /// canceller's render reference contains them and the mic does not send them back).
    /// Silent while deafened; each stream has its own gain (`set_track_volume`).
    fn mix_share(&mut self, out: &mut [f32], now_ms: u64, n: usize) {
        let deaf = self.deafened;
        for t in &mut self.share {
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
    }

    /// Share streams playing now (tests).
    #[cfg(test)]
    pub(crate) fn share_streams(&self) -> usize {
        self.share.len()
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
            ai_noise_suppression: false,
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
    #[cfg_attr(not(dpdfnet_model), ignore = "needs scripts/fetch-dpdfnet.ps1")]
    fn ai_noise_suppression_runs_in_the_call_and_switches_off() {
        let (mut p, _ev) = processor();
        p.set_processing(true, true, true);
        let mut rx = start(&mut p, 4);
        assert!(p.call.as_ref().unwrap().denoise.is_some());
        run(&mut p, 0, 100, speech);
        assert!(!drain(&mut rx).is_empty(), "the call still sends");
        p.set_processing(true, true, false);
        assert!(p.call.as_ref().unwrap().denoise.is_none());
        p.set_processing(true, true, true);
        assert!(p.call.as_ref().unwrap().denoise.is_some());
        // noise suppression off turns the AI one off too
        p.set_processing(true, false, true);
        assert!(p.call.as_ref().unwrap().denoise.is_none());
        run(&mut p, 100, 50, speech);
        assert!(!drain(&mut rx).is_empty());
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

    fn tone(secs: f32) -> Arc<ClipPcm> {
        let n = (48_000.0 * secs) as usize;
        Arc::new(ClipPcm::from_samples((0..n).map(|i| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin()).collect()))
    }

    fn finished(ev: &mut mpsc::UnboundedReceiver<AudioEvent>) -> Vec<u64> {
        events(ev).into_iter().filter_map(|e| if let AudioEvent::ClipFinished(id) = e { Some(id) } else { None }).collect()
    }

    #[test]
    fn soundboard_while_muted_is_flagged_and_loud_on_its_own_track() {
        let (mut p, mut ev) = processor();
        let mut rx = start(&mut p, 8);
        p.set_muted(true);
        p.play_clip(tone(1.0), 7).unwrap();
        run(&mut p, 0, 120, |_| 0.0);
        let pkts = drain(&mut rx);
        assert_eq!(pkts.len(), 50, "1 s clip = 50 packets, no DTX, voice muted");
        let mut dec = LibOpus.decoder().unwrap();
        let mut pcm = vec![0f32; 960];
        let mut energy = 0.0;
        let hs: Vec<AudioHeader> = pkts.iter().map(|b| AudioHeader::parse(b).unwrap().0).collect();
        assert!(hs.windows(2).all(|w| w[1].seq == w[0].seq.wrapping_add(1)));
        assert!(hs[0].flags & AF_TALKSPURT != 0);
        for b in &pkts {
            let (h, opus) = AudioHeader::parse(b).unwrap();
            assert_eq!(h.track, track_id(2, KIND_SOUND), "the soundboard track of short id 2");
            assert_eq!(h.epoch, 7);
            assert!(h.flags & AF_SOUNDBOARD != 0);
            assert!(h.flags & AF_SPEAKING == 0, "never speaking");
            assert!(h.level < 20, "level byte {}", h.level);
            dec.decode(opus, &mut pcm).unwrap();
            energy += pcm.iter().map(|x| x * x).sum::<f32>();
        }
        assert!(energy / (50.0 * 960.0) > 0.01, "clip carries energy");
        assert_eq!(finished(&mut ev), vec![7]);
        // after the clip: muted again, nothing sent
        run(&mut p, 120, 50, |_| 0.0);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_clip_over_speech_leaves_the_voice_stream_alone() {
        let (mut p, _ev) = processor();
        let mut rx = start(&mut p, 4);
        run(&mut p, 0, 50, speech);
        p.play_clip(tone(0.5), 1).unwrap();
        run(&mut p, 50, 100, speech);
        let hs: Vec<AudioHeader> = drain(&mut rx).iter().map(|b| AudioHeader::parse(b).unwrap().0).collect();
        let (voice, sound): (Vec<&AudioHeader>, Vec<&AudioHeader>) = hs.iter().partition(|h| h.track == 4);
        assert!(sound.iter().all(|h| h.track == track_id(1, KIND_SOUND) && h.flags & AF_SOUNDBOARD != 0));
        assert_eq!(sound.len(), 25, "0.5 s clip");
        assert!(voice.len() >= 60, "voice keeps going: {}", voice.len());
        assert!(voice.iter().all(|h| h.flags & AF_SOUNDBOARD == 0), "no clip mixed into the voice");
        assert!(voice.windows(2).all(|w| w[1].seq == w[0].seq.wrapping_add(1)), "voice seq untouched by the clip");
    }

    #[test]
    fn stopping_a_clip_ends_sending_and_playout_at_once() {
        let (mut p, mut ev) = processor();
        let mut rx = start(&mut p, 8);
        p.set_muted(true);
        p.play_clip(tone(2.0), 3).unwrap();
        p.play_clip(tone(2.0), 4).unwrap();
        run(&mut p, 0, 20, |_| 0.0);
        let before = drain(&mut rx).len();
        assert_eq!(before, 10);
        p.stop_clip(3);
        p.stop_clip(99); // unknown: ignored
        run(&mut p, 20, 10, |_| 0.0);
        assert_eq!(finished(&mut ev), vec![3], "only the stopped clip ends");
        assert!(p.has_local_voices(), "clip 4 still plays");
        p.stop_all_clips();
        let mut out = vec![0f32; FRAME_10MS];
        // the fade is 5 ms: the next 10 ms frame ends silent
        p.render(&mut out, 300, true);
        assert!(out[STOP_RAMP..].iter().all(|x| x.abs() < 1e-6), "faded out");
        assert!(out[..STOP_RAMP].windows(2).all(|w| (w[1] - w[0]).abs() < 0.05), "no click");
        run(&mut p, 31, 20, |_| 0.0);
        assert_eq!(finished(&mut ev), vec![4]);
        assert!(!p.has_local_voices());
        let after = drain(&mut rx).len();
        assert!(after <= 6, "{after} packets after the stop (one fade frame each time)");
        run(&mut p, 51, 20, |_| 0.0);
        assert!(drain(&mut rx).is_empty(), "nothing after both stopped");
    }

    #[test]
    fn my_room_soundboard_gain_scales_local_playout_only() {
        let (mut p, _ev) = processor();
        let mut rx = start(&mut p, 8);
        p.set_muted(true);
        p.set_local_sound_gain(0.0);
        p.play_clip(tone(0.5), 1).unwrap();
        let mut out = vec![0f32; FRAME_10MS];
        let mut buf = vec![0f32; FRAME_10MS];
        let mut local = 0.0f32;
        for f in 0..60 {
            p.render(&mut out, f * 10, true);
            local += out.iter().map(|x| x * x).sum::<f32>();
            buf.fill(0.0);
            p.capture(&mut buf, f * 10);
        }
        assert!(local < 1e-6, "silent here");
        assert_eq!(drain(&mut rx).len(), 25, "still sent in full");
    }

    #[test]
    fn play_clip_errors() {
        let (mut p, mut ev) = processor();
        let clip = Arc::new(ClipPcm::from_samples(vec![0.1; 4800]));
        assert!(matches!(p.play_clip(clip.clone(), 1), Err(AudioError::NotInCall)));
        let _rx = start(&mut p, 4);
        p.set_deafened(true);
        assert!(matches!(p.play_clip(clip.clone(), 2), Err(AudioError::Deafened)));
        assert_eq!(finished(&mut ev), vec![1, 2], "a refused clip is over at once");
        p.set_deafened(false);
        assert!(p.play_clip(clip, 3).is_ok());
    }

    /// Energy `b` plays from `a`'s muted clip, with these gains on a's two tracks.
    fn heard_clip_energy(voice_gain: f32, sound_gain: f32) -> f32 {
        let (mut a, _eva) = processor();
        let (mut b, _evb) = processor();
        let mut rx_a = start(&mut a, 4);
        let _rx_b = start(&mut b, 8);
        b.set_track_volume(4, voice_gain);
        b.set_track_volume(track_id(1, KIND_SOUND), sound_gain);
        a.play_clip(tone(1.0), 1).unwrap();
        let mut cap = vec![0f32; FRAME_10MS];
        let mut out = vec![0f32; FRAME_10MS];
        let mut energy = 0.0f32;
        for f in 0..150usize {
            let now = f as u64 * 10;
            a.render(&mut out, now, true);
            cap.iter_mut().enumerate().for_each(|(i, s)| *s = speech(f * FRAME_10MS + i));
            a.capture(&mut cap, now);
            for pkt in drain(&mut rx_a) {
                b.push_packet(pkt, now + 30);
            }
            b.render(&mut out, now + 30, true);
            energy += out.iter().map(|x| x * x).sum::<f32>();
            cap.fill(0.0);
            b.capture(&mut cap, now + 30);
        }
        energy
    }

    #[test]
    fn a_receiver_sets_soundboard_volume_apart_from_voice() {
        let both = heard_clip_energy(1.0, 1.0);
        let voice_only = heard_clip_energy(1.0, 0.0);
        let sound_only = heard_clip_energy(0.0, 1.0);
        let none = heard_clip_energy(0.0, 0.0);
        assert!(none < 1e-6, "{none}");
        assert!(voice_only > 0.5 && sound_only > 5.0, "voice {voice_only}, clip {sound_only}");
        assert!((both - voice_only - sound_only).abs() < 0.5 * both, "two separate tracks: {both} vs {voice_only} + {sound_only}");
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

    /// The end of a talkspurt and lone DTX comfort-noise frames must fade out, not jump to zero
    /// (a jump is an audible click on every pause).
    #[test]
    fn remote_audio_fades_into_pauses_without_a_click() {
        let (mut a, _eva) = processor();
        let (mut b, _evb) = processor();
        let mut rx_a = start(&mut a, 4);
        let _rx_b = start(&mut b, 8);
        let mut cap = vec![0f32; FRAME_10MS];
        let mut out = vec![0f32; FRAME_10MS];
        // a quiet room after the speech: low noise, so DTX sends comfort-noise refresh frames
        let mut seed = 1u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 * 0.004 - 0.002
        };
        let mut prev = 0f32;
        let mut max_jump = 0f32;
        for f in 0..500usize {
            let now = f as u64 * 10;
            a.render(&mut out, now, true);
            for (i, s) in cap.iter_mut().enumerate() {
                let n = f * FRAME_10MS + i;
                *s = if f < 100 { speech(n) } else { noise() };
            }
            a.capture(&mut cap, now);
            for pkt in drain(&mut rx_a) {
                b.push_packet(pkt, now + 30);
            }
            b.render(&mut out, now + 30, true);
            if f >= 90 {
                for &x in &out {
                    max_jump = max_jump.max((x - prev).abs());
                    prev = x;
                }
            } else if let Some(&x) = out.last() {
                prev = x;
            }
            cap.fill(0.0);
            b.capture(&mut cap, now + 30);
            b.tick(now + 30);
        }
        assert!(max_jump < 0.03, "largest sample step {max_jump} around and after the pause");
    }

    /// Share audio from a stereo encoder: played through its own stream (not a voice track),
    /// never lights speaking, follows its gain and deafen, and is in the AEC render path.
    #[test]
    fn share_audio_plays_as_its_own_stream() {
        use tincan_core::media::{KIND_SHARE_AUDIO, track_id};
        let (mut p, mut ev) = processor();
        let _rx = start(&mut p, 4);
        let mut enc = LibOpus.stereo_encoder(&crate::codec::StereoSettings::default()).unwrap();
        let track = track_id(3, KIND_SHARE_AUDIO);
        let mut pkt = vec![0u8; 1000];
        let mut out = vec![0f32; FRAME_10MS];
        let mut energy = |p: &mut Processor, from: usize, frames: usize| -> f32 {
            let mut e = 0.0;
            for f in from..from + frames {
                let now = f as u64 * 10;
                if f % 2 == 0 {
                    let k = f / 2;
                    let pcm: Vec<f32> = (0..960)
                        .flat_map(|i| {
                            let t = (k * 960 + i) as f32 / 48_000.0;
                            let s = 0.3 * (2.0 * std::f32::consts::PI * 300.0 * t).sin();
                            [s, -s * 0.5]
                        })
                        .collect();
                    let n = enc.encode(&pcm, &mut pkt).unwrap();
                    let h = AudioHeader { flags: 0, track, seq: k as u16, ts: (k * 960) as u32, level: 20, epoch: 7 };
                    p.push_packet(h.packet(&pkt[..n]), now);
                }
                p.render(&mut out, now + 5, true);
                if f > from + 20 {
                    e += out.iter().map(|x| x * x).sum::<f32>();
                }
                p.tick(now);
            }
            e
        };
        let loud = energy(&mut p, 0, 200);
        assert!(loud > 1.0, "share audio plays: {loud}");
        assert_eq!(p.share_streams(), 1);
        assert!(p.tracks.is_empty(), "not a voice track");
        assert!(!events(&mut ev).iter().any(|e| matches!(e, AudioEvent::Speaking { .. })), "share audio never lights speaking");
        p.set_track_volume(track, 0.0);
        let muted = energy(&mut p, 200, 100);
        assert!(muted < 1e-6, "volume 0: {muted}");
        p.set_track_volume(track, 1.0);
        p.set_deafened(true);
        let deaf = energy(&mut p, 300, 100);
        assert!(deaf < 1e-6, "deafened: {deaf}");
        p.set_deafened(false);
        assert!(energy(&mut p, 400, 100) > 0.5, "back after undeafen");
        // Unwatching removes the stream; a bounded number of streams exist at once.
        p.remove_track(track);
        assert_eq!(p.share_streams(), 0);
        for s in 0..10u16 {
            let h = AudioHeader { flags: 0, track: track_id(s, KIND_SHARE_AUDIO), seq: 0, ts: 0, level: 20, epoch: 7 };
            p.push_packet(h.packet(&pkt[..20]), 6000);
        }
        assert_eq!(p.share_streams(), MAX_SHARE_STREAMS);
        assert!(p.tracks.is_empty());
        p.stop_call();
        assert_eq!(p.share_streams(), 0);
    }

    #[test]
    fn loss_hint_maps_to_bitrate_and_perc() {
        assert_eq!(loss_perc_for(0), 5);
        assert_eq!(loss_perc_for(50), 20);
        assert_eq!(bitrate_for_loss(10), 32_000);
        assert_eq!(bitrate_for_loss(11), 24_000);
    }
}
