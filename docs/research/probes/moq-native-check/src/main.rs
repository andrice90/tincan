// Throwaway: capture a display via moq-video's WGC backend, encode H.264 (Kind from argv),
// report process CPU time, and dump access units for a WebView2 decode test.
// usage: moq-native-check <auto|hw|sw> <display-id|primary> [w h] [dump-path]
use std::io::Write;
use std::time::{Duration, Instant};

use moq_video::Rate;
use moq_video::capture;
use moq_video::encode::{Config, Encoder, Gop, Kind};

fn cpu_time() -> Duration {
    use moq_video::windows::Win32::Foundation::FILETIME;
    use moq_video::windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u).unwrap() };
    let f = |t: FILETIME| ((t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64) * 100;
    Duration::from_nanos(f(k) + f(u))
}

fn is_idr(p: &[u8]) -> bool {
    p.windows(4).any(|w| w[0..3] == [0, 0, 1] && w[3] & 0x1f == 5)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let kind = match args.get(1).map(|s| s.as_str()) { Some("hw") => Kind::Hardware, Some("sw") => Kind::Software, _ => Kind::Auto };
    let mut cc = capture::Config::default();
    cc.source = match args.get(2).map(|s| s.as_str()) { Some("primary") | None => capture::Source::Display(None), Some(id) => capture::Source::Display(Some(id.to_string())) };
    if let (Some(w), Some(h)) = (args.get(3).and_then(|s| s.parse().ok()), args.get(4).and_then(|s| s.parse().ok())) {
        cc.width = Some(w);
        cc.height = Some(h);
    }
    let dump = args.get(5).cloned();
    cc.framerate = Some(Rate::new(30, 1)?);
    let cpu0 = cpu_time();
    let mut stream = capture::open(&cc).await?;
    println!("capture opened: {} reported {}x{} fps={:?}", stream.label(), stream.width(), stream.height(), stream.framerate().map(|r| r.as_f64()));
    // Read one frame to learn the real size of what WGC delivers.
    let first = stream.read().await?.expect("frame");
    let sz = first.size();
    println!("first frame surface size {}x{}", sz.width, sz.height);
    let (w, h) = (sz.width & !1, sz.height & !1);
    let mut ec = Config::new(w, h, Rate::new(30, 1)?);
    ec.bitrate = Some(moq_net::bandwidth::Rate::from_kbps(2500));
    ec.gop = Gop::keyframe_every(Duration::from_secs(4), Rate::new(30, 1)?);
    ec.kind = kind;
    let mut enc = Encoder::new(&ec)?;
    println!("encoder backend: {}", enc.name());
    let mut out_file = dump.as_ref().map(|p| std::fs::File::create(p).unwrap());
    let start = Instant::now();
    let cpu_start = cpu_time();
    let (mut frames, mut aus, mut bytes, mut keys, mut max_au) = (0u32, 0u32, 0usize, 0u32, 0usize);
    let mut pending = Some(first);
    let mut write = |payload: &[u8], ts_us: u64, keys: &mut u32| {
        let k = is_idr(payload);
        if k { *keys += 1; }
        if let Some(f) = out_file.as_mut() {
            f.write_all(&(payload.len() as u32).to_le_bytes()).unwrap();
            f.write_all(&[k as u8]).unwrap();
            f.write_all(&ts_us.to_le_bytes()).unwrap();
            f.write_all(payload).unwrap();
        }
    };
    while start.elapsed() < Duration::from_secs(5) {
        let frame = if let Some(f) = pending.take() { f } else {
            match tokio::time::timeout(Duration::from_millis(500), stream.read()).await {
                Ok(Ok(Some(f))) => f,
                Ok(Ok(None)) => break,
                Ok(Err(e)) => { println!("capture error {e}"); break }
                Err(_) => continue,
            }
        };
        frames += 1;
        if frames == 60 { enc.cut()?; }
        for au in enc.encode(&frame)? {
            aus += 1;
            bytes += au.payload.len();
            max_au = max_au.max(au.payload.len());
            let ts = (au.timestamp.as_micros()) as u64;
            write(&au.payload, ts, &mut keys);
        }
    }
    for au in enc.flush()? { aus += 1; bytes += au.payload.len(); write(&au.payload, au.timestamp.as_micros() as u64, &mut keys); }
    let secs = start.elapsed().as_secs_f64();
    let cpu = cpu_time() - cpu_start;
    println!("frames in={frames} AUs out={aus} in {secs:.2}s ({:.1} fps), keyframes={keys}, max AU {max_au} B, avg {:.0} kbps", frames as f64 / secs, bytes as f64 * 8.0 / secs / 1000.0);
    println!("process CPU over encode loop: {:.0} ms for {secs:.2}s wall = {:.1}% of one core (setup cpu {:.0} ms)", cpu.as_secs_f64() * 1000.0, cpu.as_secs_f64() / secs * 100.0, (cpu_start - cpu0).as_secs_f64() * 1000.0);
    Ok(())
}
