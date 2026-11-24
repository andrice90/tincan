//! A/B of the two noise suppressors on a recording: writes the WAV through each chain.
//!
//! ```text
//! cargo run --release -p tincan-audio --example denoise_ab -- noisy.wav [out_dir=.]
//! ```
//!
//! The input must be 48 kHz (mono, or stereo averaged to mono); convert others first, e.g.
//! `ffmpeg -i in.m4a -ar 48000 -ac 1 noisy.wav`. Outputs, next to each other for listening:
//! - `<name>.webrtc.wav`: high-pass, WebRTC noise suppression, AGC2 (today's chain, without the
//!   echo canceller)
//! - `<name>.dpdfnet.wav`: high-pass and AGC2, then DPDFNet (the AI chain)
//! - `<name>.dpdfnet-only.wav`: DPDFNet alone, nothing else (for comparing with the reference
//!   Python implementation)
//!
//! It also prints DPDFNet's time per 10 ms frame on this machine.
//! Needs the model: run `scripts/fetch-dpdfnet.ps1` first.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use sonora::config::{
    AdaptiveDigital, GainController2, HighPassFilter, MaxProcessingRate, NoiseSuppression, Pipeline,
};
use sonora::{AudioProcessing, Config, StreamConfig};
use tincan_audio::denoise::{self, Denoiser, HOP};

/// The capture side of `src/apm.rs` without the echo canceller.
fn apm(high_pass: bool, noise_suppression: bool, gain_control: bool) -> AudioProcessing {
    let config = Config {
        pipeline: Pipeline {
            maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz,
            ..Default::default()
        },
        high_pass_filter: high_pass.then(HighPassFilter::default),
        noise_suppression: noise_suppression.then(NoiseSuppression::default),
        gain_controller2: gain_control.then(|| GainController2 {
            input_volume_controller: false,
            adaptive_digital: Some(AdaptiveDigital::default()),
            fixed_digital: Default::default(),
        }),
        ..Default::default()
    };
    let sc = StreamConfig::new(48_000, 1);
    AudioProcessing::builder()
        .config(config)
        .capture_config(sc)
        .render_config(sc)
        .build()
}

fn run_apm(apm: &mut AudioProcessing, frame: &mut [f32]) {
    let input = frame.to_vec();
    if let Err(e) = apm.process_capture_f32(&[&input[..]], &mut [frame]) {
        eprintln!("apm: {e}");
    }
}

fn read_48k_mono(path: &Path) -> anyhow::Result<Vec<f32>> {
    let mut r =
        hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = r.spec();
    if spec.sample_rate != 48_000 {
        bail!(
            "{} is {} Hz; convert it to 48 kHz first (ffmpeg -i in -ar 48000 -ac 1 out.wav)",
            path.display(),
            spec.sample_rate
        );
    }
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>()
                .map(|s| s.map(|s| s as f32 * scale))
                .collect::<Result<_, _>>()?
        }
    };
    let ch = usize::from(spec.channels.max(1));
    Ok(raw
        .chunks(ch)
        .map(|c| c.iter().sum::<f32>() / ch as f32)
        .collect())
}

fn write(path: &Path, samples: &[f32]) -> anyhow::Result<()> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(path, spec)?;
    for s in samples {
        w.write_sample((s.clamp(-1.0, 1.0) * 32767.0).round() as i16)?;
    }
    w.finalize()?;
    println!("wrote {}", path.display());
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let input = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: denoise_ab noisy.wav [out_dir]")?,
    );
    let out_dir = std::env::args()
        .nth(2)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    if !denoise::available() {
        bail!("this build has no DPDFNet model: run scripts/fetch-dpdfnet.ps1 and rebuild");
    }
    let t = Instant::now();
    denoise::preload();
    println!("model load: {:?}", t.elapsed());

    let mut x = read_48k_mono(&input)?;
    x.resize(x.len().div_ceil(HOP) * HOP, 0.0);

    let mut webrtc_apm = apm(true, true, true);
    // the app's AI chain: high-pass and AGC2, then the model
    let mut ai_apm = apm(true, false, true);
    let mut ai = Denoiser::new().context("model")?;
    let mut only = Denoiser::new().context("model")?;
    let (mut webrtc_out, mut ai_out, mut only_out) = (
        Vec::with_capacity(x.len()),
        Vec::with_capacity(x.len()),
        Vec::with_capacity(x.len()),
    );
    let mut times: Vec<Duration> = Vec::with_capacity(x.len() / HOP);
    for frame in x.as_chunks::<HOP>().0 {
        let mut a = frame.to_vec();
        run_apm(&mut webrtc_apm, &mut a);
        webrtc_out.extend_from_slice(&a);

        let mut b = frame.to_vec();
        run_apm(&mut ai_apm, &mut b);
        let t = Instant::now();
        ai.process(&mut b);
        times.push(t.elapsed());
        ai_out.extend_from_slice(&b);

        let mut c = frame.to_vec();
        only.process(&mut c);
        only_out.extend_from_slice(&c);
    }

    // Drop the STFT hop (the reference implementation's output lines up the same way, which the
    // parity check relies on). The model's own 40 ms lag stays in the DPDFNet files.
    let ai_out = &ai_out[HOP.min(ai_out.len())..];
    let only_out = &only_out[HOP.min(only_out.len())..];
    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("out");
    std::fs::create_dir_all(&out_dir)?;
    write(&out_dir.join(format!("{stem}.webrtc.wav")), &webrtc_out)?;
    write(&out_dir.join(format!("{stem}.dpdfnet.wav")), ai_out)?;
    write(&out_dir.join(format!("{stem}.dpdfnet-only.wav")), only_out)?;

    times.sort();
    let mean = times.iter().sum::<Duration>() / times.len().max(1) as u32;
    let pct = |p: usize| times[(times.len() * p / 100).min(times.len() - 1)];
    println!(
        "dpdfnet per 10 ms frame: mean {:.2} ms, p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms ({} frames; {:.0}% of one core)",
        mean.as_secs_f64() * 1e3,
        pct(50).as_secs_f64() * 1e3,
        pct(99).as_secs_f64() * 1e3,
        times.last().copied().unwrap_or_default().as_secs_f64() * 1e3,
        times.len(),
        mean.as_secs_f64() * 1e3 / 10.0 * 100.0,
    );
    Ok(())
}
