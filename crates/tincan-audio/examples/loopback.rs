//! Manual test: microphone -> engine A -> simulated network -> engine B -> speakers.
//!
//! Use headphones: without them B's playback reaches A's microphone and howls (A's echo
//! canceller only knows A's own playout).
//!
//! ```text
//! cargo run -p tincan-audio --example loopback -- [seconds=20] [loss_pct=0] [max_jitter_ms=0]
//! ```

use bytes::Bytes;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use std::time::Duration;
use tincan_audio::{AudioConfig, AudioEngine, AudioEvent};

fn arg(i: usize, default: u64) -> u64 {
    std::env::args().nth(i).and_then(|a| a.parse().ok()).unwrap_or(default)
}

fn print_events(name: &'static str, mut rx: tokio::sync::mpsc::UnboundedReceiver<AudioEvent>) {
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                AudioEvent::MicLevel(_) => {}
                other => println!("[{name}] {other:?}"),
            }
        }
    });
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let seconds = arg(1, 20);
    let loss = arg(2, 0).min(100) as f64 / 100.0;
    let jitter = arg(3, 0);

    let devices = AudioEngine::devices()?;
    for d in devices.inputs.iter().filter(|d| d.is_default) {
        println!("microphone: {}", d.name);
    }
    for d in devices.outputs.iter().filter(|d| d.is_default) {
        println!("speaker:    {}", d.name);
    }

    let (a, a_events) = AudioEngine::new(AudioConfig::default())?;
    let (b, b_events) = AudioEngine::new(AudioConfig::default())?;
    print_events("A", a_events);
    print_events("B", b_events);

    let (a_tx, mut a_rx) = tokio::sync::mpsc::channel::<Bytes>(256);
    let (b_tx, _b_rx) = tokio::sync::mpsc::channel::<Bytes>(256);
    b.set_muted(true); // B only listens
    b.start_call(8, 0, b_tx)?;
    a.start_call(4, 0, a_tx)?;
    a.set_loss_hint((loss * 100.0) as u8);

    let net = {
        let b = b.clone();
        tokio::spawn(async move {
            let mut rng = StdRng::seed_from_u64(1);
            while let Some(pkt) = a_rx.recv().await {
                if loss > 0.0 && rng.random_bool(loss) {
                    continue;
                }
                let delay = if jitter > 0 { rng.random_range(0..=jitter) } else { 0 };
                let b = b.clone();
                tokio::spawn(async move {
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                    }
                    b.push_packet(pkt);
                });
            }
        })
    };

    println!("talking for {seconds} s (loss {:.0} %, jitter 0..{jitter} ms)...", loss * 100.0);
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    a.stop_call();
    b.stop_call();
    net.abort();
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(())
}
