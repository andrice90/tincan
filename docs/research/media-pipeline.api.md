All snippets below compiled and/or ran in the scratch projects under C:/Users/andri/AppData/Local/Temp/claude/d3bbc0386af2202e/d39a4a52-4271-48fa-9546-1005d34914ef/scratchpad/research/media-pipeline/ (aec-native, iroh-dgram, moq-native-check, wv2-probe).

1) iroh 1.5 media connection config (iroh-dgram; Cargo: iroh = "1.5", noq-proto = "1.5", bytes = "1")
```rust
use iroh::endpoint::{QuicTransportConfig, ConnectOptions, presets};
use iroh::{Endpoint, RelayMode};
fn media_transport() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .datagram_send_buffer_size(16 * 1024)          // size to ~150 ms of aggregate audio in production
        .datagram_receive_buffer_size(Some(256 * 1024))
        .congestion_controller_factory(std::sync::Arc::new(noq_proto::congestion::Bbr3Config::default()))
        .build()
}
// dial side, per-connection config:
let conn = ep.connect_with_opts(host_addr, b"tincan/media/1",
        ConnectOptions::new().with_transport_config(media_transport())).await? // -> Connecting
    .await?;                                                                    // -> Connection
// accept side, per-connection config:
let sc = std::sync::Arc::new(ep.create_server_config_builder(vec![b"tincan/media/1".to_vec()])
        .set_transport_config(media_transport()).build());
let conn = incoming.accept_with(sc)?.await?;
// datagrams (audio) and path info:
let _max = conn.max_datagram_size();       // Option<usize>; measured 1162..1288 on loopback, read at runtime
conn.send_datagram(bytes::Bytes::from(pkt))?; // drops OLDER queued datagrams when the buffer is full
let d: bytes::Bytes = conn.read_datagram().await?;
for p in conn.paths().iter() { let (_sel, _relay, _rtt) = (p.is_selected(), p.is_relay(), p.rtt()); }
// stream per GOP (video):
let mut s = conn.open_uni().await?;
s.set_priority(group_seq as i32)?;   // set BEFORE first write
s.write_all(&frame_bytes).await?;    // or write_chunk(Bytes)
s.finish()?;                         // or s.reset(0u32.into())? to drop the group
```
Endpoint for the test: `Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled).transport_config(cfg).alpns(vec![ALPN.to_vec()]).bind().await?`. Production would use presets::N0, which has relays.

2) sonora 0.3 APM (aec-native)
```rust
use sonora::config::{EchoCanceller, NoiseSuppression};
use sonora::{AudioProcessing, Config, StreamConfig};
let sc = StreamConfig::new(48_000, 1);          // sc.num_frames() == 480 (10 ms)
let mut apm = AudioProcessing::builder()
    .config(Config { echo_canceller: Some(EchoCanceller::default()),
                     noise_suppression: Some(NoiseSuppression::default()), ..Default::default() })
    .capture_config(sc).render_config(sc).build();
apm.process_render_f32(&[&played_10ms[..]], &mut [&mut scratch[..]]).unwrap();  // exact samples sent to the speaker
apm.process_capture_f32(&[&mic_10ms[..]], &mut [&mut clean[..]]).unwrap();
```

3) unsafe-libopus 0.2 encoder (aec-native)
```rust
use unsafe_libopus::{OPUS_APPLICATION_VOIP, OPUS_OK, OPUS_SET_BITRATE_REQUEST, OPUS_SET_DTX_REQUEST,
  OPUS_SET_INBAND_FEC_REQUEST, OPUS_SET_PACKET_LOSS_PERC_REQUEST, OPUS_SET_COMPLEXITY_REQUEST,
  opus_encoder_create, opus_encoder_ctl_impl, opus_encode_float, opus_decoder_create, opus_decode_float, varargs};
let mut err = 0i32;
let enc = unsafe { opus_encoder_create(48_000, 1, OPUS_APPLICATION_VOIP, &mut err) };
for (req, v) in [(OPUS_SET_BITRATE_REQUEST, 32_000), (OPUS_SET_INBAND_FEC_REQUEST, 1),
                 (OPUS_SET_PACKET_LOSS_PERC_REQUEST, 10), (OPUS_SET_DTX_REQUEST, 1), (OPUS_SET_COMPLEXITY_REQUEST, 7)] {
    assert_eq!(unsafe { opus_encoder_ctl_impl(enc, req, varargs![v]) }, OPUS_OK);
}
let n = unsafe { opus_encode_float(enc, pcm960.as_ptr(), 960, out.as_mut_ptr(), out.len() as i32) };
// PLC: opus_decode_float(dec, std::ptr::null(), 0, pcm.as_mut_ptr(), 960, 0)
// FEC: decode the NEXT packet with decode_fec = 1 into the lost slot (not exercised here)
```

4) moq-video 0.1.23 capture + encode standalone (moq-native-check; Cargo: moq-video = { version = "=0.1.23", default-features = false, features = ["capture"] } (+ "openh264" for SW fallback), moq-net = "0.3.23")
```rust
use moq_video::{capture, Rate, encode::{Config, Encoder, Gop, Kind}};
let displays = capture::displays().await?;              // id like "display:0", name, width, height
let mut cc = capture::Config::default();
cc.source = capture::Source::Display(Some("display:1".into()));   // or Source::Window(id) from capture::windows()
cc.framerate = Some(Rate::new(30, 1)?);                  // width/height requests were NOT honored on WGC
let mut stream = capture::open(&cc).await?;
let frame = stream.read().await?.unwrap();               // frame.size() -> Size{width,height}
let mut ec = Config::new(w, h, Rate::new(30, 1)?);
ec.bitrate = Some(moq_net::bandwidth::Rate::from_kbps(2500));
ec.gop = Gop::keyframe_every(std::time::Duration::from_secs(4), Rate::new(30, 1)?);
ec.kind = Kind::Hardware;                                // backend name: "mediafoundation"
let mut enc = Encoder::new(&ec)?;
for au in enc.encode(&frame)? { /* au.payload: Bytes (Annex-B, AUD+SPS+PPS+IDR on keyframes), au.timestamp */ }
enc.cut()?;                                              // forced IDR on the next frame (verified)
enc.set_bitrate(moq_net::bandwidth::Rate::from_kbps(1500))?;
// keyframe flag: scan payload for 00 00 01 NAL with (b & 0x1f) == 5
```

5) Tauri 2.12 raw Channel to the webview, and the WebCodecs decode that worked (wv2-probe)
```rust
#[tauri::command]
fn video(on_data: tauri::ipc::Channel<tauri::ipc::InvokeResponseBody>) {
    std::thread::spawn(move || { /* per AU: */ let _ = on_data.send(tauri::ipc::InvokeResponseBody::Raw(msg_vec)); });
}
```
```js
// tauri.conf.json: app.withGlobalTauri = true (or @tauri-apps/api Channel)
const ch = new window.__TAURI__.core.Channel();
ch.onmessage = (buf) => {            // keep SYNCHRONOUS; no await before configure/decode
  const u8 = new Uint8Array(buf); const key = u8[0] === 1;
  const ts = Number(new DataView(buf).getBigUint64(1, true)); const data = u8.subarray(9);
  if (!configured) { dec.configure({ codec: codecFromSps(data) /* 'avc1.'+hex(sps[1..4]) e.g. avc1.640032 */,
                                     hardwareAcceleration: 'prefer-hardware', optimizeForLatency: true }); configured = true; }
  dec.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta', timestamp: ts, data }));  // Annex-B, no description
};
window.__TAURI__.core.invoke('video', { onData: ch });
```
WebCodecs encode config accepted by WebView2 157 (fallback path): `{codec:'avc1.640028', width:1920, height:1080, framerate:30, bitrate:2_500_000, bitrateMode:'constant' (or 'quantizer' with prefer-hardware only), latencyMode:'realtime', hardwareAcceleration:'prefer-hardware', contentHint:'detail', avc:{format:'annexb'}}`. Opus: `{codec:'opus', sampleRate:48000, numberOfChannels:1, bitrate:32000, opus:{frameDuration:20000, application:'voip', signal:'voice', useinbandfec:true, usedtx:true, packetlossperc:10, complexity:9}}`.

Tauri probe setup that worked without npm: tauri = "2.12", tauri-build = "2", tauri.conf.json build.frontendDist "../dist", app.withGlobalTauri true, capabilities/default.json {"windows":["main"],"permissions":["core:default"]}, icons/icon.ico + icon.png.