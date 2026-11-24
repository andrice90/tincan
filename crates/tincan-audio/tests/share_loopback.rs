//! Real Windows process loopback for screen share audio. Ignored by default (it plays a quiet
//! 1.5 s tone on the default speaker):
//! `cargo test -p tincan-audio --test share_loopback -- --ignored --nocapture --test-threads=1`.
//!
//! This process plays a tone through the audio engine and captures its own process tree, the
//! way a window share captures the shared app; then it captures everything except its own
//! tree, the way a display share leaves Tincan out.

#![cfg(windows)]

use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tincan_audio::{AudioConfig, AudioEngine, ClipPcm, LibOpus, OpusCodec, ShareAudio, ShareAudioSource};
use tincan_core::media::AudioHeader;
use tokio::sync::mpsc;

fn tone() -> Arc<ClipPcm> {
    let samples = (0..48_000 * 3 / 2).map(|i| 0.05 * (2.0 * std::f32::consts::PI * 523.0 * i as f32 / 48_000.0).sin()).collect();
    Arc::new(ClipPcm::from_samples(samples))
}

/// Captures `source` while this process plays the tone; returns (packets, mean energy per sample).
fn capture_while_playing(source: ShareAudioSource) -> (usize, f32) {
    let (engine, _events) = AudioEngine::new(AudioConfig::default()).unwrap();
    let (tx, mut rx) = mpsc::channel::<Bytes>(1000);
    let share = ShareAudio::start(source, tx).expect("process loopback opens");
    engine.play_local(tone());
    std::thread::sleep(Duration::from_millis(1_400));
    share.stop();
    drop(engine);
    let mut dec = LibOpus.stereo_decoder().unwrap();
    let mut out = vec![0f32; 1920];
    let (mut n, mut e, mut samples) = (0usize, 0f32, 0usize);
    while let Ok(p) = rx.try_recv() {
        let (h, opus) = AudioHeader::parse(&p).expect("an audio datagram");
        assert_eq!(h.flags & tincan_core::media::AF_SPEAKING, 0);
        dec.decode(opus, &mut out).expect("decodes");
        n += 1;
        e += out.iter().map(|x| x * x).sum::<f32>();
        samples += out.len();
    }
    (n, e / samples.max(1) as f32)
}

#[test]
#[ignore = "plays a quiet tone on the default speaker and captures it"]
fn process_loopback_captures_own_tree_and_excludes_it() {
    let _ = tracing_subscriber::fmt().with_env_filter("tincan_audio=debug").with_test_writer().try_init();
    let me = std::process::id();
    let (n_in, e_in) = capture_while_playing(ShareAudioSource::ProcessTree(me));
    println!("include own tree: {n_in} packets, mean energy {e_in:.2e}");
    assert!(n_in >= 55, "about 50 packets a second, silence filled: {n_in}");
    // 0.05 amplitude sine: 1.25e-3 per sample when nothing scales it.
    assert!(e_in > 1e-4, "our own tone is captured: {e_in:.2e}");
    let (n_ex, e_ex) = capture_while_playing(ShareAudioSource::AllExcept(me));
    println!("exclude own tree: {n_ex} packets, mean energy {e_ex:.2e}");
    assert!(n_ex >= 55, "{n_ex}");
    assert!(e_ex < e_in / 10.0, "our tone is left out ({e_ex:.2e} vs {e_in:.2e}); other apps may play");
}
