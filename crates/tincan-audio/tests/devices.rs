//! Smoke tests against the real audio devices of the machine. Ignored by default:
//! `cargo test -p tincan-audio --test devices -- --ignored --nocapture`.

use std::sync::Arc;
use std::time::Duration;
use tincan_audio::{AudioConfig, AudioEngine, AudioEvent, ClipPcm};

fn logs() {
    let _ = tracing_subscriber::fmt().with_env_filter("tincan_audio=debug").with_test_writer().try_init();
}

#[test]
#[ignore = "needs real audio devices"]
fn enumerate_devices() {
    let devs = AudioEngine::devices().expect("enumerate devices");
    println!("inputs:");
    for d in &devs.inputs {
        println!("  {}{} [{}]", if d.is_default { "* " } else { "  " }, d.name, d.id);
    }
    println!("outputs:");
    for d in &devs.outputs {
        println!("  {}{} [{}]", if d.is_default { "* " } else { "  " }, d.name, d.id);
    }
    assert!(devs.inputs.iter().filter(|d| d.is_default).count() <= 1);
    assert!(devs.outputs.iter().filter(|d| d.is_default).count() <= 1);
    assert!(devs.inputs.iter().chain(&devs.outputs).all(|d| !d.id.is_empty() && !d.name.is_empty()));
}

/// Opens the default speaker and plays 300 ms of digital silence (inaudible), then checks that
/// the engine closed it again without errors.
#[tokio::test]
#[ignore = "needs a real output device"]
async fn output_opens_plays_silence_and_closes() {
    logs();
    let (e, mut events) = AudioEngine::new(AudioConfig::default()).unwrap();
    e.play_local(Arc::new(ClipPcm::from_samples(vec![0.0; 48_000 * 3 / 10])));
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    while let Ok(ev) = events.try_recv() {
        assert!(!matches!(ev, AudioEvent::DeviceError(_)), "{ev:?}");
    }
}

/// Opens the default microphone for one second of mic test and prints the levels.
/// Opens the microphone: run it only on purpose.
#[tokio::test]
#[ignore = "opens the real microphone"]
async fn mic_test_reports_levels() {
    logs();
    let (e, mut events) = AudioEngine::new(AudioConfig::default()).unwrap();
    e.mic_test(true);
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    e.mic_test(false);
    let mut levels = Vec::new();
    while let Ok(ev) = events.try_recv() {
        match ev {
            AudioEvent::MicLevel(l) => levels.push(l),
            AudioEvent::DeviceError(m) => panic!("device error: {m}"),
            _ => {}
        }
    }
    println!("levels: {levels:?}");
    assert!(levels.len() >= 10);
}
