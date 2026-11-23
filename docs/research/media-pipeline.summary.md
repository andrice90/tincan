# media-pipeline

## Summary
Recommendation: a native Rust media engine plus host-forwarded iroh QUIC, with the webview used only to decode and draw video. That combines option C for capture, audio and encode with option B's forwarding SFU on the host. Audio (WASAPI via cpal, sonora AEC3/NS, Opus) never touches WebView2. Screen share uses Windows.Graphics.Capture and the Media Foundation hardware H.264 encoder in Rust. The encoded frames cross Tauri IPC into a WebCodecs VideoDecoder.

All of the following were compiled or run on this machine (Windows 11, MSVC, WebView2 157, Tauri 2.12):
- sonora 0.3, unsafe-libopus 0.2 and cpal 0.18 build in 20 s with no C toolchain. AEC3 costs about 0.5% of one core. Opus encode costs 223-232 us per 20 ms.
- webrtc-audio-processing 2.1 `bundled` fails here because meson is missing.
- moq-video 0.1.23 does WGC capture plus MF hardware H.264 at 1920x1200 for 0.3% process CPU. Its Annex-B output decodes in the WebView2 VideoDecoder with no description: 109 of 109 frames, p50 0.4 ms.
- Tauri Channel<Raw> carries 30 x 100 KB/s and 60 x 200 KB/s with inter-arrival within a few ms of nominal and no loss.
- iroh 1.5 datagrams drop the oldest packets when a small send buffer is full. max_datagram_size varies between 1162 and 1288 B. Stream priorities, BBR3 and per-connection transport config all compile.

Why not the alternatives:
- **A (WebRTC mesh):** without TURN, about 22% of conferences fail (callstats, 2015-16), with no fallback to iroh relays. A mesh sharer uploads 5 copies of the share.
- **B fully in the webview:** echo cancellation of AudioContext playback in Edge/WebView2 is uncertain. Tauri cannot disable background throttling on Windows. The getDisplayMedia picker has a live deadlock bug with Tauri windows (WebView2Feedback #5763).
- **D (host webview relays tracks):** WebView2 157 has no RTCRtpEncodedSource, so forwarding means re-encoding. A local WebRTC loopback to str0m is possible but rebuilds an SFU and breaks congestion control across hops.

Wire protocol: own a thin protocol on iroh modelled on moq-lite (one QUIC stream per GOP, priorities, drop the group until the next keyframe), with audio on datagrams. The published moq-tokio[iroh] + moq-net + iroh 1.5 do resolve to a single iroh. But moq main has already removed that iroh adapter, and iroh-live pins moq by git rev.

Top risks:
- About 10% of paths are relay-only, and n0's public relays are rate-limited and meant for development.
- The host's upload carries 4-5x the share.
- sonora's quality in real rooms is unproven.

## Recommendation
VERDICT: build option "C+B-hybrid".
- Audio is fully native in Rust: capture, AEC/NS/AGC, Opus, jitter buffer, PLC, mixing and playout.
- Screen capture and encode are native: Windows.Graphics.Capture plus the Media Foundation hardware H.264 MFT.
- The transport is a Tincan-owned thin protocol over iroh QUIC. Each member has one media connection to the host, and the host is a forwarding SFU with no transcoding.
- The webview only decodes and draws video with WebCodecs VideoDecoder. Encoded access units reach it over a Tauri Channel<InvokeResponseBody::Raw>.
- 1:1 DM calls use the same protocol over the direct iroh connection between the two peers. Each side is "host" of a 2-member room, its forwarding table is empty, and there is no migration (the call ends when either side leaves).

WHY, PER OPTION
- A, WebRTC mesh in the webview, rejected:
  - It needs STUN and, for about 22% of conferences, TURN (callstats.io 2015-16). Those are servers. Without TURN those calls simply fail, and WebRTC cannot use iroh's relay fallback.
  - In a mesh the sharer uploads N-1 copies (5 x 2.6 Mbps = 13 Mbps for one 1080p share). It also likely runs one encoder per RTCPeerConnection (reasoned, not measured).
- B, WebCodecs plus JS jitter buffers in the webview, rejected for audio and capture:
  - Echo cancellation of AudioContext/AudioWorklet playback depends on Chromium's ChromeWideEchoCancellation and SystemLoopbackAsAecReference defaults. Both are enabled in Chromium main; the loopback reference has an added_delay_ms=170 param. Edge-based WebView2 may differ: a Zoom forum thread (Aug 2026) reports echo in Edge, and there is no detection API.
  - Tauri cannot disable background throttling on Windows. Sharers and listeners minimise during calls.
  - The getDisplayMedia picker has a live deadlock with Tauri 2/wry windows (WebView2Feedback #5763, Oct 2026), and there is no custom picker (tauri #11272).
  - Keep B only as the video DECODE side, which is verified working, and as a software-encode fallback: VP8, VP9 SW and AV1 SW encoders are supported in WebView2 157.
- C, native: chosen.
  - Verified builds on MSVC with no extra toolchain: sonora 0.3, unsafe-libopus 0.2, cpal 0.18, moq-audio 0.1.23 [aec] and moq-video 0.1.23 [capture, openh264].
  - Measured costs: AEC3 50-58 us per 10 ms; Opus encode about 0.23 ms per 20 ms; WGC+MF H.264 at 1920x1200 costs 0.3% process CPU.
  - Audio keeps working whatever the webview's state, and the AEC render reference is exactly the samples we play.
  - Prior art: n0's own iroh-live is built on the same stack (moq-audio/moq-video with sonora AEC).
  - Fallback if sonora disappoints in real rooms: webrtc-audio-processing 2.1 `bundled`. This is not drop-in. It failed here because meson is missing; its README lists meson, ninja, pkg-config and clang/gcc.
- D, hybrids, rejected:
  - D1, the host webview relays tracks: WebView2 157 has no RTCRtpEncodedSource (verified undefined), so every forwarded track would be decoded and re-encoded per outgoing PeerConnection.
  - D2, RTCRtpScriptTransform passthrough: present, but it only edits frames inside an existing sender or receiver. It cannot originate frames and gives nothing over WebCodecs.
  - D3, local WebRTC loopback (webview to str0m 0.26 on 127.0.0.1) with RTP over iroh: workable, and gets NetEQ and Chrome's AEC. But congestion control and NACK break across hops (the browser's GCC sees localhost), so we would have to synthesise REMB/TWCC or drive setParameters maxBitrate, plus rewrite SSRC and sequence numbers at the host. That is more code than C.
- Wire protocol, own versus moq-net:
  - Published moq-tokio 0.19.37 [iroh] + moq-net 0.3.23 + iroh 1.5 resolve to a single iroh 1.5.0 and cargo check passes. But moq main (1fd8509, 2026-11-22) has removed moq-tokio's iroh.rs and `iroh` feature.
  - moq-lite was renamed to moq-net in 2026, and moq-native to moq-tokio.
  - iroh-live pins moq main by git rev, and its node "carries no traffic between its links", so it is not a forwarder.
  - So own a small protocol (about 1-2k LOC) that copies moq-lite's model. If you use moq-video's Encoder and capture as libraries, they pull in moq-net and hang types (Timestamp, bandwidth::Rate), so pin with `=0.1.23`.

CONNECTIONS
- Use a dedicated media connection per member to the host on ALPN "tincan/media/1". Give it its own QuicTransportConfig via ConnectOptions::with_transport_config / Incoming::accept_with (compiled):
  - datagram_send_buffer_size of about 150 ms of the connection's aggregate audio, minimum 2 KiB. The noq default is 1 MiB, which means seconds of stale audio under congestion.
  - datagram_receive_buffer_size 64 KiB.
  - congestion controller: Bbr3Config (delay-based, compile-verified only). A/B it against the default Cubic.
- Inside that connection, a bi control stream carries length-prefixed messages:
  - Join, TrackAnnounce{track_id, kind, codec, w, h}
  - Subscribe / Unsubscribe{track_id, max_bps}
  - KeyframeRequest{track_id}
  - TargetBitrate{track_id, bps}
  - ReceiverReport (every 1 s): per track rx, lost, late, jitter_p95, groups_dropped, decodeQueue
  - Mute{audio}, Leave
- Make track_id stable per (member short id, kind), so that host migration needs no remap.

AUDIO SPEC
- Capture and playback: WASAPI shared mode via cpal 0.18, 48 kHz mono f32, 10 ms processing frames; resample with rubato if the device rate differs.
- Processing: sonora AudioProcessing with echo_canceller, noise_suppression and AGC2/HPF. The render reference is the exact post-volume mix handed to the output callback (process_render_f32), in 10 ms frames.
- Encoder: Opus 48 kHz mono, 20 ms (960 samples), OPUS_APPLICATION_VOIP, SIGNAL_VOICE, VBR at 32 kbps (range 16-48).
  - complexity 7 (c9 measured at about 0.23 ms per 20 ms).
  - INBAND_FEC=1, with PACKET_LOSS_PERC set to the reported loss, clamped 5-20.
  - DTX=1.
- Transport: QUIC datagram, unreliable and unordered (Opus at 20 ms is at most about 160 B; max_datagram_size was 1162-1288 B measured, so check it at runtime).
- Header, 12 bytes, big-endian; the host forwards it verbatim:
  - [0] u8 0xA1 (audio v1)
  - [1] u8 flags: bit0 speaking (VAD), bit1 talkspurt start (first packet after DTX), bit2 FEC present
  - [2..4] u16 track_id
  - [4..6] u16 seq: +1 per packet sent, and DTX does not consume seq, so a seq gap means loss and a ts jump with contiguous seq means DTX
  - [6..10] u32 ts, 48 kHz sample clock from a random origin
  - [10] u8 level, RFC 6464-style 0..127 = -dBov
  - [11] u8 host-epoch low bits, for stale detection after migration
  - then the Opus packet
- Jitter buffer, per track in Rust:
  - min 40 ms, start 60 ms, max 200 ms (300 ms when the path is relayed).
  - Target = max(min, P95 of relative arrival delay over a 2 s window + 1 frame). Use NetEQ-style histogram parameters as in moq's audio-jitter.md: 20 ms buckets, 0.983 forget factor.
  - Grow immediately on underrun.
  - Shrink only when the level exceeds target + 20 ms for 1 s or more. Drop one 20 ms frame during VAD-off/low-energy frames, or apply up to 10% WSOLA speed-up; prefer re-anchoring at talkspurt start. The same rule absorbs device clock drift.
- PLC:
  - If the next packet is buffered, decode the lost frame with decode_fec=1.
  - Otherwise run Opus PLC (null data) for up to 5 frames (100 ms), then fade to silence.
  - DTX gaps play silence; Opus already produced its CNG frame.
- Mixing: per-user gain, sum, soft limiter, a 10-20 ms output ring. The same buffer feeds the AEC reference. Route UI sounds through this mixer so the AEC sees them.
- Speaking indicator: the sender sets flag bit0 and the level from post-APM frame energy with a 200-300 ms hangover. Receivers light the ring after 2 consecutive speaking packets and hold it 300 ms. The host can rank active speakers from the level byte.
- Mute: stop sending datagrams and send Mute{true}. Keep the APM running locally for a "you are muted" hint, with a privacy option to close the device.
  - Deafen = mute + stop playout.
  - Moderator mute = the host stops forwarding the track and notifies.
- Expected mouth-to-ear: about 120-200 ms at 20-40 ms one-way per hop. This is an estimate.

VIDEO (SCREEN SHARE) SPEC
- Capture:
  - Use WGC through moq_video::capture::{displays, windows, open}, with a custom picker in the Tincan UI.
  - Measured: WGC delivers frames only on change (about 22 fps on a mostly static desktop), so every stage must handle variable frame rate.
  - Measured: requested width/height is ignored (a 3440x1440 display stayed 3440x1440). Downscale large displays on the GPU (D3D11 VideoProcessor / MF video processor). moq-video's Frame::resize is a CPU path on Windows.
- Encoder: MF hardware H.264 (moq_video::encode::Encoder, Kind::Hardware), Annex-B with AUD/SPS/PPS in band (measured level 5.0, avc1.640032 at 1920x1200).
  - CBR, CODECAPI_AVLowLatencyMode, no B-frames.
  - GOP 4 s (Gop::keyframe_every) plus on-demand IDR via Encoder::cut(), which is verified to work.
  - Live bitrate via Encoder::set_bitrate().
- Modes:
  - "text" (default): up to about 2.3 Mpx native (else downscale to 1920 wide), up to 15 fps, 1.2 Mbps target, 2.5 Mbps cap.
  - "motion": up to 1920x1080 at 30 fps (60 only if the host budget allows), 3 Mbps target, 4.5 Mbps cap.
  - Below 600 kbps fall to 720p at 10 fps. Floor 300 kbps.
- Fallbacks when there is no hardware MFT:
  - openh264 (measured 24% of one core at 1920x1200 and about 22 fps; 84 ms per frame at 3440x1440). Patent coverage applies only to Cisco's binaries.
  - Or Microsoft's software H.264 MFT (needs a non-HARDWARE MFTEnumEx; moq-video only enumerates hardware).
  - Or webview WebCodecs encode: avc1.640028 / vp09.00.10.08 SW / vp8, all verified supported. Config: {latencyMode:'realtime', bitrateMode:'quantizer' (HW only) or 'constant', hardwareAcceleration:'prefer-hardware', contentHint:'detail' or 'motion' (accepted), avc:{format:'annexb'}}. Avoid avc1.42E01F (level 3.1 rejects 1080p) and use level 4.0 or higher.
- Transport: one QUIC uni stream per GOP group. Call set_priority(group_seq) BEFORE the first write; noq applies changed priorities to queued data only at the stream's next turn.
  - Stream header: u8 0xB1, u16 track_id, u32 group_seq, u8 codec (1 = H264 Annex-B), u8 flags.
  - Then frames: varint len | u32 ts (90 kHz) | u8 fflags (bit0 keyframe, set from a NAL scan for type 5, since moq-video's Encoded has only timestamp+payload; bit1 discardable) | access unit.
- Host forwarder, per viewer:
  - When a new upstream group arrives: reset() the viewer's previous group stream if it is unfinished, open a new uni stream at priority = group_seq, and cut-through frames as they arrive.
  - Each viewer has a bounded mpsc into a writer task using write_chunk(Bytes).
  - Drop rule: if try_send overflows, or the oldest queued frame is more than 300 ms old, or the queue holds more than max(2 frames, 250 ms of bitrate), then reset the group stream, set waiting_for_key, skip deltas until the next group, and send a coalesced KeyframeRequest upstream.
  - Never use temporal-layer dropping on the MF path: nothing shows MF exposes L1T2.
  - Audio is never queued behind video, since datagrams are separate.
  - Cache the current group from its keyframe (bounded to 4 s or about 2 MB). A joining viewer gets the cached group immediately, plus a fresh KeyframeRequest if that group is older than 1.5 s.
- Keyframe request (PLI-like) flow:
  - The viewer detects a frame gap within a group, a group reset, a decoder error, or decodeQueueSize > 3, and sends KeyframeRequest on the control stream.
  - The host serves the cached keyframe if it is fresh. Otherwise it forwards at most one KeyframeRequest per track per second to the sharer.
  - The sharer calls Encoder::cut() (MF CODECAPI_AVEncVideoForceKeyFrame).
- Bitrate control (AIMD):
  - Viewers send ReceiverReports.
  - The host computes target = min(0.85 x min viewer throughput estimate, (0.7 x host_uplink - audio_fanout) / remote_viewers) and sends TargetBitrate.
  - The sharer applies x0.7 immediately on any group drop, and +10% every 4 s when clean.
  - For audio: raise Opus PACKET_LOSS_PERC with loss, and drop to 24 kbps when loss exceeds 10%.
- Viewer:
  - Rust sends Raw messages over a Tauri Channel. Measured inter-arrival is within a few ms of nominal, with no loss or reordering.
  - Use a SYNCHRONOUS onmessage. An async handler that awaited before configure() caused out-of-order configure and decode errors in testing.
  - VideoDecoder({codec: 'avc1.' + SPS bytes, hardwareAcceleration: 'prefer-hardware', optimizeForLatency: true}) with no description (verified 109 of 109 frames, p50 0.4 ms decode, NV12).
  - Draw the latest VideoFrame to a canvas on requestAnimationFrame / requestVideoFrameCallback and close() frames promptly.
  - On visibilitychange to hidden, Unsubscribe the video so the host stops spending uplink. Audio is unaffected because it is native.

HOST MIGRATION, RELAY AND BACKPRESSURE POLICY
- Migration: members re-dial the new host's media ALPN; streams resume with stable track_ids. The sharer emits a keyframe on reconnect. Receivers reset decoders and keep jitter-buffer state per track_id.
- Host election must weigh measured uplink and require direct (non-relay) paths to most members. Detect a relay path with conn.paths() -> is_selected() / is_relay(), which compiled.
- A relayed member is never host. As a viewer it gets video at 800 kbps or less, 720p, and jitter max 300 ms. As a sharer it is capped at 1 Mbps. Audio is unchanged.
- Blob/file transfers on other connections compete with media without coordination. During calls, token-bucket iroh-blobs to 50% of measured uplink or less, and deprioritise seeding on the host.
- Media is protected hop-by-hop only, so the host sees plaintext. Add SFrame-style per-frame E2EE later if needed.

BUDGET, 6-person room, one 1080p share at 2.5 Mbps (about 2.6 Mbps on the wire), Opus at about 60 kbps on the wire per active talker (32 kbps + 12 B header + about 56 B QUIC/UDP/IPv4 per packet at 50 pps). All figures are estimates unless marked measured.
- Viewer: up about 0.06 Mbps when talking; down 2.6 Mbps + up to 0.3 Mbps of audio.
- Sharer (not host): up about 2.66 Mbps; down about 0.3 Mbps.
- Host (not sharer): up 4 x 2.6 = 10.4 Mbps + audio fan-out (0.5 Mbps with 2 talkers, 1.5 Mbps worst) = about 11-12 Mbps; down about 2.9 Mbps.
- Host who is also the sharer: up about 13 Mbps + audio.
- Text mode at 1.2 Mbps: host up about 5.5 Mbps.
- Mesh equivalent: the sharer uploads 13 Mbps regardless.
- CPU per member:
  - Audio about 3% of one core: AEC3+NS 0.5-0.6% (measured), Opus encode about 1.1% at c9 (measured), 5 decodes about 0.7% (measured, about 28 us per frame), I/O and mix about 0.5% (estimated).
  - Sharer: 0.3% process CPU on mostly static content with MF hardware (measured); motion content about 1-3% (estimated).
  - Viewer: hardware decode p50 0.4 ms per frame (measured); compositing about 1-3% (estimated).
  - Host forwarding about 1-3% of one core at 12 Mbps egress (estimated, not measured).
  - Memory: Rust media state under 5 MB (estimated).

## Versions
- iroh 1.5.0 crates.io 2026-11-19; uses noq 1.5.0 / noq-proto 1.5.0 (Cubic default, Bbr3Config available)
- noq / noq-proto 1.5.0 n0's QUIC fork under iroh
- sonora 0.3.0 pure-Rust WebRTC APM (AEC3/NS/AGC2), 2026-11-07; built and ran on MSVC
- unsafe-libopus 0.2.0 libopus 1.3.1 transpiled, pure Rust; built and ran
- cpal 0.18.2 WASAPI; compiled
- moq-audio 0.1.23 features aec/capture/playback compile on MSVC; API tied to moq-mux/hang tracks
- moq-video 0.1.23 WGC capture + MF HW H.264 encoder; standalone capture/Encoder API ran
- moq-net 0.3.23 formerly moq-lite
- moq-tokio 0.19.37 'iroh' feature uses web-transport-iroh ^0.7; removed in moq main @1fd8509
- web-transport-iroh 0.8.4 latest; moq-tokio still on 0.7.0
- hang 0.21.24 
- iroh-live git 747b1bd (2026-11-19) not on crates.io; pins moq main by git rev
- webrtc-audio-processing 2.1.0 bundled build fails without meson on Windows
- aec3 (RubyBit) 0.4.0 not tested
- str0m 0.26.0 only for hybrid D3, not used
- tauri 2.12.7 probe built against 2.12.x
- WebView2 Runtime 157.0.4341.60 probe UA Edg/157
- windows (windows-rs) 0.62.2 used by moq-video
- opus (bindings) 0.4.0 alternative via opusic-sys; not tested
- Rust toolchain 1.99.1 MSVC 

## Risks
- Relay-only paths, about 1 in 10 per the iroh docs: n0's public relays are rate-limited (limits unpublished) and meant for development and testing. Media over them can degrade or be cut off, which undercuts the 'no servers' promise. Mitigations: elect hosts with direct paths, cap relayed members, rely on iroh's portmapper (UPnP/PCP), or eventually run a Tincan relay.
- The host's upload is the bottleneck: one 1080p share costs 4 x 2.6 Mbps of host upload (5x if the host shares), plus up to about 1.5 Mbps of audio fan-out. Host election must use measured uplink, and video bitrate must be capped at roughly host_uplink / viewers.
- sonora is v0.3 (Nov 2026), an AI-assisted port with no neural residual echo estimator. Real-room double-talk, laptop speaker nonlinearity and USB/Bluetooth clock drift are unevaluated. The C++ fallback (webrtc-audio-processing bundled) needs meson, ninja, pkg-config and clang/gcc on Windows build machines.
- moq ecosystem churn: moq-lite was renamed to moq-net and moq-native to moq-tokio in 2026, and moq main removed the moq-tokio iroh adapter. Pin with '=' if you use moq-video/moq-audio, or vendor them.
- Hardware MFT availability and quality vary by GPU vendor and driver, and VMs or GPU-less machines have none. openh264 costs about 24% of one core at 1920x1200 and 84 ms per frame at 3440x1440. Statically compiled openh264 is not covered by Cisco's patent licence. moq-video does not enumerate Microsoft's software H.264 MFT.
- WGC ignores requested output size, and moq-video's Frame::resize is a CPU path on Windows, so ultrawide and 4K sources need a GPU scaler (D3D11 VideoProcessor) or the encode cost and keyframes (126 KB at 3440x1440) balloon.
- Echo-cancellation reference gaps: any sound played by the webview (notifications, video previews) bypasses the native AEC reference. Route all call-relevant audio through the native mixer.
- WebView2 hidden-window throttling cannot be disabled from Tauri on Windows. Never put media pacing or timers in JS; render with requestAnimationFrame only while visible.
- WebView2 getDisplayMedia picker deadlock (#5763) if any code path falls back to webview capture.
- noq datagram-versus-stream scheduling under congestion and BBR3 behaviour for media are unmeasured. noq set_priority requeue caveat: priority changes reach already-queued data only at the stream's next turn.
- Media and blob connections compete for the same bottleneck without coordination, so seeding can bufferbloat calls unless rate-limited during calls.
- No end-to-end encryption: the host forwards plaintext media (QUIC is hop-by-hop).
- Bluetooth headsets switching to the 16 kHz HFP profile when the mic opens, and default-device changes mid-call, require device re-open and resampler/AEC reset logic.

## Open
- What are n0's public relay rate limits in numbers, and are they acceptable for 60 kbps audio or 1 Mbps video over relay in production? If not, does Tincan run its own relay?
- How good is sonora AEC3 in real conditions (laptop speakers, double-talk, USB mic + HDMI speaker drift) compared with webrtc-audio-processing (C++)? This needs recorded-room tests.
- Does WebView2 (via wry) set IsVisible=false on minimize, and what exactly happens to requestAnimationFrame and Channel delivery while hidden for more than 5 minutes? Re-run the probe with the window hidden.
- How do noq DATAGRAM frames get scheduled relative to STREAM frames when the congestion window is full? Does audio stay low-latency while a video group stream is backlogged? Measure on a shaped link.
- Do vendor MF hardware H.264 encoders (NVIDIA, AMD, Intel) honour CODECAPI temporal layer count, so the host could drop T1 frames instead of whole groups?
- Can the Microsoft software H.264 MFT be added as a royalty-covered fallback (moq-video only enumerates MFT_ENUM_FLAG_HARDWARE), and what does it cost in CPU at 1080p?
- BBR3 vs Cubic in noq 1.5 for the media connection: latency under competing blob transfers.
- What are the actual host forwarding CPU and memory at 12 Mbps of egress on Windows?
- What are the Edge/WebView2 157 defaults for ChromeWideEchoCancellation and SystemLoopbackAsAecReference? This only matters if any call audio is ever played in the webview.
- Will WGC's yellow capture border, and the borderless capability on Windows 11, need a manifest or capability request in a Tauri-packaged app?
