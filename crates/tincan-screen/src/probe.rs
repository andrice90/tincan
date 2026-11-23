//! One-time probe for a hardware H.264 encoder.

use std::sync::OnceLock;

use moq_video::Rate;
use moq_video::encode::{Config, Encoder, Kind};

static AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Whether this machine has a hardware H.264 encoder (a Media Foundation
/// hardware MFT) that can be opened.
///
/// The first call opens and releases one encoder on a short-lived thread of
/// its own (COM there is always MTA, whatever apartment the caller is in) and
/// blocks the caller meanwhile: measured 240 ms on an RTX 5080 machine, most
/// of it Media Foundation start-up, so call it from a blocking-friendly thread
/// (for example `tokio::task::spawn_blocking`) the first time. The answer is
/// cached for the life of the process, so later calls are free. Screen share
/// is unavailable when this is false.
pub fn hardware_encoder_available() -> bool {
    if let Some(known) = AVAILABLE.get() {
        return *known;
    }
    match probe() {
        Some(found) => *AVAILABLE.get_or_init(|| found),
        // The probe itself could not run (thread spawn failed): answer no for
        // now without caching, so a later call can try again.
        None => false,
    }
}

fn probe() -> Option<bool> {
    let spawned = std::thread::Builder::new()
        .name("tincan-screen-probe".into())
        .spawn(|| {
            let rate = match Rate::new(30, 1) {
                Ok(rate) => rate,
                Err(err) => {
                    tracing::warn!(%err, "invalid probe frame rate");
                    return false;
                }
            };
            let mut config = Config::new(1280, 720, rate);
            config.kind = Kind::Hardware;
            match Encoder::new(&config) {
                Ok(encoder) => {
                    tracing::info!(encoder = encoder.name(), "hardware H.264 encoder available");
                    true
                }
                Err(err) => {
                    tracing::info!(%err, "no hardware H.264 encoder; screen share unavailable");
                    false
                }
            }
        });
    match spawned {
        Ok(handle) => match handle.join() {
            Ok(found) => Some(found),
            Err(_) => {
                tracing::warn!("encoder probe thread panicked");
                Some(false)
            }
        },
        Err(err) => {
            tracing::warn!(%err, "could not start the encoder probe thread");
            None
        }
    }
}
