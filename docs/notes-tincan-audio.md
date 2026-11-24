# tincan-audio notes

Implementation notes for whoever wires `tincan-audio` into `tincan-app`. The design is in `architecture.md` ("Audio", "Soundboard"). This file records the threading model, the choices made where the docs left room, and what the app has to supply.

## Threading

- `AudioEngine::new` spawns one thread, `tincan-audio`, at `THREAD_PRIORITY_HIGHEST`. That thread owns the APM, the encoder, the jitter buffers and decoders, the mixer and every cpal stream, and creates and drops the streams itself. The handle is `Clone + Send + Sync`: a command channel, a few atomics and an unpark. Dropping the last clone stops the thread and joins it, which takes a few ms while devices close.
- cpal callbacks only convert samples, downmix or upmix, and move them through a wait-free SPSC ring of `AtomicU32`. They store one latency figure and unpark the engine thread. They never allocate, lock or log. Stream errors cross over a bounded channel as `cpal::ErrorKind`.
- Wake-ups come from input data, output demand and commands. A 10 ms clock runs only for custom IO, or during a call whose input or output device is missing, so soundboard clips still go out and jitter buffers keep draining.
- With no call, no mic test and no UI sound playing, the thread parks with no timeout and uses 0 CPU. Devices close 500 ms after the last UI sound.
- `push_packet` stamps the arrival time itself and queues the packet without unparking; the next device tick drains the queue. Packets are dropped when no call is running.

## Signal path

- Capture: device -> mono -> rubato to 48 kHz -> input gain -> mic-test meter -> sonora APM, 10 ms frames.
- The APM runs AEC3, NS (Moderate), AGC2 (adaptive digital; the analog volume controller is off), a high-pass filter and a 48 kHz internal rate.
- Encode: two APM frames make one 20 ms Opus frame, VOIP/voice, 32 kbps VBR, complexity 7, FEC, DTX.
- Soundboard clips go out on their own Opus stream, never mixed into the voice: track `track_id(short_id, KIND_SOUND)` (the voice track's short id, kind 2), 40 kbps, no DTX, gain 0.7 and a soft limiter, flag `AF_SOUNDBOARD`, never `AF_SPEAKING`. It shares the voice's sample clock and sends only while a clip plays (a gap is a pause: seq contiguous, ts jump). So a receiver keeps a separate jitter buffer and gain per person for clips, and the voice keeps DTX while a clip plays. If its encoder can't be made, clips still play locally and are not sent.
- `play_clip` returns an id; `AudioEvent::ClipFinished(id)` follows when the clip has been sent and played, was stopped, was cut (deafen, `stop_call`, the oldest of 8 when a ninth starts) or was refused on the engine thread. `stop_clip(id)` and `stop_clips()` fade the local and the sent copy out over 5 ms (`STOP_RAMP`), so a stop doesn't click. `set_local_sound_gain` scales only my own clips here (the room's soundboard volume).
- Render: each track's jitter buffer -> Opus decode/FEC/PLC -> per-track gain -> sum, plus clip voices -> output gain -> soft limiter. That exact signal goes to the APM render reference, then is resampled for the device if needed.
- Output is opened at 48 kHz when Windows accepts it (shared-mode render auto-converts). Capture always uses the native format.

## Screen share audio (src/share.rs, src/loopback.rs)

- `ShareAudio::start(source, out)` blocks until the source is open (call it from a blocking thread), then runs the `tincan-share-audio` thread. `stop()` and `Drop` join it. It never touches the engine thread.
- Sources: `ProcessTree(pid)` and `AllExcept(pid)` are Windows process loopback (`loopback.rs`: `ActivateAudioInterfaceAsync` on `VAD\Process_Loopback`, our own 48 kHz stereo float format with `AUTOCONVERTPCM`, polled every 10 ms on an MTA thread). `TestTone` is a 220/330 Hz stereo tone at real-time pace.
- The `PROPVARIANT` that carries the activation params is wrapped in `ManuallyDrop`: the windows crate gives it a `Drop` that calls `PropVariantClear`, which would free the borrowed blob (it was heap corruption on the first run).
- Packets: 20 ms stereo frames, Opus music application, 128 kbps VBR, no FEC, no DTX, payload capped at 1000 bytes. The header's track and epoch are 0 (tincan-net stamps them); only the first packet has `AF_TALKSPURT`, none has `AF_SPEAKING`. Silence fills the gap when the source lags the wall clock by more than 40 ms; more than 100 ms of backlog is dropped.
- Helpers for the app: `window_process_id(hwnd)` and `is_self_or_ancestor(pid)` (a Toolhelp process snapshot; an unreadable list counts as an ancestor).
- Playback: `Processor` routes `KIND_SHARE_AUDIO` tracks to share streams (at most `MAX_SHARE_STREAMS` = 4, apart from the 64 voice and soundboard tracks, `MAX_TRACKS`): own jitter buffer, the mono decoder (it downmixes the stereo packets), per-track gain, silent while deafened, no speaking events. They are mixed before the output gain and limiter, so they are in the AEC render reference. `remove_track` and `stop_call` drop them. Stereo playout would need a stereo output bus (ring, resampler, sink, and a stereo or downmixed AEC reference).
- Stereo codec: `OpusCodec::stereo_encoder` and `stereo_decoder` (default methods refuse, so other codec implementations still compile). Frame sizes are per channel; buffers are interleaved.

## Jitter buffer (src/jitter.rs)

- Target delay: P95 of the relative transit over the last 2 s plus 20 ms, clamped to min/max.
- Underrun: an empty buffer is concealed without advancing the playout point. If the missing frame then arrives late, it plays: the concealment became extra delay, and the target is boosted by 20 ms for 2 s. If later frames arrive first, the concealed frames count as lost and are skipped.
- Pauses: a timestamp jump with contiguous sequence numbers is a DTX or mute pause. If the pause drained the buffer, the playout re-anchors one target delay after the talkspurt's first packet; otherwise the pause plays as silence.
- Shrink: when the level stays above target + 20 ms for 1 s, one quiet frame is dropped (no speaking flag, or level byte 50 or more), or one frame of a pause. After 3 s of excess any frame goes, so a continuous signal cannot keep latency high.
- PLC runs for at most 5 frames, then one faded frame, then silence. A 5 ms fade-in follows any silence.
- A sequence jump of 64 or more frames in either direction resets the stream (the sender restarted).

## Is the engine alive?

- `AudioEngine::is_running()` is false once the engine thread has stopped: a panic unwound it, or the last handle is being dropped. Every later command then fails with `AudioError::EngineStopped` (or is dropped, for the fire-and-forget ones). A stopped engine never restarts; drop it and make a new one.
- The event receiver closing outside shutdown means the same thing; `is_running` lets the app check without owning the receiver. Test: `tests/e2e.rs` `is_running_turns_false_when_the_engine_thread_dies` (a custom source that panics on the engine thread).

## AI noise suppression (DPDFNet, prototype)

- `src/denoise.rs` runs DPDFNet (Apache-2.0, github.com/ceva-ip/DPDFNet), the 48 kHz streaming model `dpdfnet2_48khz_hr` (2.6M parameters), with tract (pure Rust, no ONNX Runtime). The STFT and overlap-add around it follow the reference `StreamEnhancer`: 960-point FFT, Vorbis window, 480-sample hop.
- Latency: the output is 50 ms behind the input (`denoise::DELAY`): 10 ms for the STFT plus 40 ms inside the model, measured by cross-correlating clean speech with the output. The reference implementation has the same lag; its README only mentions the 10 ms. WebRTC noise suppression adds about 7 ms.
- The model is not in git. `scripts/fetch-dpdfnet.ps1` downloads it (pinned Hugging Face revision and sha256) to `crates/tincan-audio/models/`, and build.rs embeds it (+10 MB) when present. Without it `denoise::available()` is false, the switch is hidden and calls keep WebRTC noise suppression.
- Chain with `ai_noise_suppression`: high-pass, AEC3 and AGC2 in sonora with its noise suppression off, then DPDFNet, then VAD and Opus. AGC2 after the model instead (a second, gain-only APM) was tried and not kept: the residual noise floor was better on two test files and worse on the third. The soundboard stream never goes through it. Switching it on starts a fresh denoiser state; switching off drops it.
- tract: run every frame with a fresh plan state (`plan.run`). A reused `SimpleState` keeps the GRUs' hidden state as op state and adds it to `state_in`, so the output drifts (found by the parity check below).
- Parity: on 16.5 s of noisy speech the Rust output matches the reference Python package (onnxruntime) at 69 dB SNR, the 16-bit WAV limit; model outputs alone agree to about 1e-5 relative.
- Cost on an i9-12900K: 3.1-3.6 ms per 10 ms frame (31-36 % of one core), p99 about 5 ms; under heavy other load up to 5.3 ms. ONNX Runtime (`ort` 2.0.0-rc.15) runs the same model in about half the time; tract was kept for the prototype for a pure-Rust build. opt-level 3 for the tract crates gained about 2 %, not kept. The model loads in about 140 ms, once per process, off the engine thread (`denoise::preload` in `AudioEngine::new` and `set_processing`).
- A/B on TTS speech mixed with synthetic fan noise, keyboard clicks and both (0-4 dB SNR): residual noise in a noise-only stretch, relative to the speech level, was -53 / -11 / +1 dB with DPDFNet against -2 / -1 / +5 dB with WebRTC (both chains with AGC2, which raises quiet stretches). Not yet tried on real recordings.
- A/B: `cargo run --release -p tincan-audio --example denoise_ab -- noisy.wav out_dir` writes the WebRTC chain, the DPDFNet chain and DPDFNet alone, and prints the per-frame cost. Live: `examples/loopback.rs` with `ai=1` as its fourth argument.

## What the app supplies

- `local_track` = `track_id(short_id, KIND_VOICE)`. Remote tracks are created on their first packet (64 at most: a voice and a soundboard track per person) and keyed by `header.track`. Map track to user with `track_short_id`, and the kind with `track_kind`. Packets of my own short id (either kind) are ignored.
- In DM calls there is no host to hand out short ids, so both sides must use distinct track ids. tincan-net uses the rule "the lower `UserId` takes short id 0" and exports it as `tincan_net::call_short_id(me, peer)`; `CallInfo.peer_short_id` is the peer's.
- `set_epoch(e)` after a host migration. Jitter-buffer state survives because track ids are stable.
- `set_relayed(true)` when the path to the host is relay-only (raises the jitter maximum to 300 ms).
- `set_loss_hint(pct)` from receiver reports.
- `decode_clip(bytes, max_ms)` once per clip; cache the `Arc<ClipPcm>`. The limit allows 100 ms of codec padding slack.
- Errors: `AudioError::NotInCall` displays as `not-in-voice` and `AudioError::Deafened` as `deafened` (the api.md strings). Device problems are `AudioEvent::DeviceError(sentence)` and never fail `start_call`.
- `AudioEvent::MicLevel` is 0..1 for -60..0 dBFS of the raw input after the input gain.
- `LocalSpeaking` is the transmitted speaking flag, so it is false while muted.

## Device handling

- `None` means the system default. A default-device stream that loses its device gets `StreamInvalidated` (a new default exists) or `DeviceNotAvailable` (none does), and the engine reopens it right away.
- If reopening fails, the engine emits one `DeviceError` and retries every 2 s while the device is wanted. `DeviceChanged` follows a re-route or a recovery.
- A selected device that is missing falls back to the default with a `DeviceError`.
- `set_devices` switches open streams immediately.

## Tests

- `cargo test -p tincan-audio` runs the unit tests: codec, ring, DSP, jitter buffer and the processor in virtual time (DTX, speaking, mute, two processors). Soundboard: a muted clip is 50 packets on the soundboard track; a clip over speech leaves the voice stream unflagged with contiguous seq; stop cuts one clip of two, then all, within the 5 ms fade, without a click; the local sound gain silences my playout but not what is sent; a receiver hears voice and clip apart (gain 0 on one track removes only that part).
- `tests/clips.rs` covers WAV via hound, FLAC and MP3 frames built by hand, max length, and soundboard while muted through the engine (on the soundboard track, `ClipFinished(id)`, `stop_clip` ends sending).
- `tests/e2e.rs` runs two engines over a simulated network with 5 % loss and 0-60 ms jitter for about 8 s. It also checks mic-test levels and that the engine idles.
- `tests/devices.rs` is ignored by default: device enumeration, the speaker playing silence, and a mic test, which opens the microphone.
- `examples/loopback.rs` sends the microphone through two engines to the speaker. Use headphones.
- `tests/share_loopback.rs` is ignored by default and plays a quiet 523 Hz tone for 1.5 s: this process plays it through the engine, captures its own process tree (the tone is there: mean energy 1.19e-3 against 1.25e-3 for the clean tone) and then everything except its own tree (energy 0). `cargo test -p tincan-audio --test share_loopback -- --ignored --nocapture`.
