//! Share a screen source for a few seconds and write the raw H.264 Annex-B
//! elementary stream to a file, for manual checks (`ffprobe`, `ffplay`, VLC).
//!
//! ```text
//! cargo run -p tincan-screen --example capture_to_file -- [source-id|list] [seconds] [out.h264] [text|motion]
//! ```
//!
//! Defaults: the first listed source (the primary display), 3 seconds,
//! `capture.h264` in the current directory, text mode.

use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use tincan_core::media::{annexb_nals, h264_codec_string};
use tincan_screen::{ScreenShare, ShareMode, hardware_encoder_available, sources};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let listed = sources().await?;
    if args.first().map(String::as_str) == Some("list") {
        for s in &listed {
            println!(
                "{:<22} {:<8} {:>5}x{:<5} {}",
                s.id,
                s.kind.as_str(),
                s.width,
                s.height,
                s.name
            );
        }
        return Ok(());
    }
    let source = match args.first() {
        Some(id) => id.clone(),
        None => listed.first().context("no screen sources")?.id.clone(),
    };
    let seconds: f64 = args.get(1).map(|s| s.parse()).transpose()?.unwrap_or(3.0);
    let path = args.get(2).cloned().unwrap_or_else(|| "capture.h264".into());
    let mode = match args.get(3) {
        Some(m) => ShareMode::parse(m).context("mode must be text or motion")?,
        None => ShareMode::Text,
    };

    println!("hardware encoder available: {}", hardware_encoder_available());
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).with_context(|| format!("create {path}"))?);
    let (tx, mut rx) = mpsc::channel(64);
    let started = Instant::now();
    let share = ScreenShare::start(&source, mode, tx).await?;
    println!("sharing {source} ({}) after {:?}", mode.as_str(), started.elapsed());
    let cpu_before = process_cpu();
    let sharing = Instant::now();

    let deadline = tokio::time::sleep(Duration::from_secs_f64(seconds));
    tokio::pin!(deadline);
    let (mut frames, mut keys, mut bytes, mut largest) = (0u64, 0u64, 0usize, 0usize);
    let mut codec = None;
    loop {
        tokio::select! {
            () = &mut deadline => break,
            frame = rx.recv() => {
                let Some(frame) = frame else { break };
                if frame.keyframe {
                    keys += 1;
                    if codec.is_none() {
                        codec = h264_codec_string(&frame.data);
                    }
                }
                frames += 1;
                bytes += frame.data.len();
                largest = largest.max(frame.data.len());
                file.write_all(&frame.data)?;
            }
        }
    }
    let stats = share.stats();
    let stop = Instant::now();
    share.stop();
    let stop_took = stop.elapsed();
    file.flush()?;
    drop(file);

    let secs = started.elapsed().as_secs_f64();
    let cpu = process_cpu().saturating_sub(cpu_before);
    let shared_for = sharing.elapsed().as_secs_f64();
    println!(
        "process CPU while sharing: {:.0} ms over {shared_for:.2} s = {:.1}% of one core",
        cpu.as_secs_f64() * 1000.0,
        cpu.as_secs_f64() / shared_for * 100.0
    );
    println!(
        "{frames} frames ({keys} keyframes) in {secs:.2}s, {:.0} kbps avg, largest AU {largest} B, codec {}",
        bytes as f64 * 8.0 / secs / 1000.0,
        codec.as_deref().unwrap_or("?")
    );
    println!("stats at stop: {stats:?}; stop took {stop_took:?}");

    // Verify the file opens like a decoder expects: a start code, then the
    // parameter sets (an access unit delimiter may come first).
    let head = std::fs::read(&path)?;
    let starts_with_code = head.starts_with(&[0, 0, 0, 1]) || head.starts_with(&[0, 0, 1]);
    let first_types: Vec<u8> = annexb_nals(&head[..head.len().min(4096)])
        .take(4)
        .filter_map(|n| n.first().map(|b| b & 0x1f))
        .collect();
    println!(
        "{path}: {} bytes, starts with a start code: {starts_with_code}, first NAL types: {first_types:?}",
        head.len()
    );
    let sps_first =
        first_types.first() == Some(&7) || (first_types.first() == Some(&9) && first_types.get(1) == Some(&7));
    if !starts_with_code || !sps_first {
        bail!("{path} does not start with a start code followed by an SPS");
    }
    Ok(())
}

/// User + kernel CPU time of this process so far.
fn process_cpu() -> Duration {
    use moq_video::windows::Win32::Foundation::FILETIME;
    use moq_video::windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the out pointers are valid FILETIMEs for the duration of the call.
    let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    if ok.is_err() {
        return Duration::ZERO;
    }
    let ticks = |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
    Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
}
