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
