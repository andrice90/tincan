//! Live tests against the real desktop and GPU.
//!
//! Everything that captures or encodes is `#[ignore]`: it needs an interactive
//! Windows session and a hardware H.264 encoder. Run them one at a time (one
//! WGC session and one hardware encoder per test):
//!
//! ```text
//! cargo test -p tincan-screen --test live -- --ignored --test-threads=1 --nocapture
//! ```

use std::time::{Duration, Instant};

use tincan_core::media::{VideoFrame, annexb_nals, h264_codec_string, h264_is_keyframe};
use tincan_screen::{
    EndReason, Error, MIN_WINDOW_SIDE, ScreenShare, ShareMode, SourceKind, hardware_encoder_available, sources,
};
use tokio::sync::mpsc;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "tincan_screen=debug,info".into()),
        )
        .with_test_writer()
        .try_init();
}

/// A received frame and when it arrived.
struct Received {
    at: Instant,
    frame: VideoFrame,
}

async fn collect(rx: &mut mpsc::Receiver<VideoFrame>, duration: Duration, out: &mut Vec<Received>) {
    let deadline = tokio::time::Instant::now() + duration;
    while let Ok(Some(frame)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        out.push(Received {
            at: Instant::now(),
            frame,
        });
    }
}

fn nal_types(au: &[u8]) -> Vec<u8> {
    annexb_nals(au).filter_map(|n| n.first().map(|b| b & 0x1f)).collect()
}

// ------------------------------------------------------------------ H.264 bits
//
// Just enough bitstream parsing to prove decodability without a decoder: the
// SPS gives the size and log2_max_frame_num, and every slice's frame_num must
// continue from the previous received picture (a gap means a lost reference).

fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> u32 {
        let byte = self.data.get(self.pos / 8).copied().expect("bitstream ended early");
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        u32::from(bit)
    }
    fn u(&mut self, n: u32) -> u32 {
        (0..n).fold(0, |acc, _| (acc << 1) | self.bit())
    }
    fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while self.bit() == 0 {
            zeros += 1;
            assert!(zeros < 32, "bad exp-golomb");
        }
        (1u32 << zeros) - 1 + self.u(zeros)
    }
    fn se(&mut self) -> i32 {
        let v = self.ue();
        if v % 2 == 1 {
            v.div_ceil(2) as i32
        } else {
            -((v / 2) as i32)
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Sps {
    width: u32,
    height: u32,
    log2_max_frame_num: u32,
}

fn parse_sps(nal: &[u8]) -> Sps {
    let data = rbsp(&nal[1..]);
    let mut b = Bits { data: &data, pos: 0 };
    let profile = b.u(8);
    b.u(16); // constraint flags, level
    b.ue(); // seq_parameter_set_id
    let mut chroma_format = 1;
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        chroma_format = b.ue();
        if chroma_format == 3 {
            b.u(1);
        }
        b.ue(); // bit_depth_luma_minus8
        b.ue(); // bit_depth_chroma_minus8
        b.u(1); // qpprime_y_zero_transform_bypass_flag
        assert_eq!(b.u(1), 0, "scaling matrices are not expected from MF");
    }
    let log2_max_frame_num = b.ue() + 4;
    let poc_type = b.ue();
    if poc_type == 0 {
        b.ue();
    } else if poc_type == 1 {
        b.u(1);
        b.se();
        b.se();
        for _ in 0..b.ue() {
            b.se();
        }
    }
    b.ue(); // max_num_ref_frames
    b.u(1); // gaps_in_frame_num_value_allowed_flag
    let mbs_w = b.ue() + 1;
    let map_units_h = b.ue() + 1;
    let frame_mbs_only = b.u(1);
    if frame_mbs_only == 0 {
        b.u(1);
    }
    b.u(1); // direct_8x8_inference_flag
    let (mut crop_x, mut crop_y) = (0, 0);
    if b.u(1) == 1 {
        let (l, r, t, bo) = (b.ue(), b.ue(), b.ue(), b.ue());
        let (sub_w, sub_h) = if chroma_format == 1 { (2, 2) } else { (1, 1) };
        crop_x = (l + r) * sub_w;
        crop_y = (t + bo) * sub_h * (2 - frame_mbs_only);
    }
    Sps {
        width: mbs_w * 16 - crop_x,
        height: map_units_h * 16 * (2 - frame_mbs_only) - crop_y,
        log2_max_frame_num,
    }
}

/// (frame_num, is reference) of the first slice in an access unit.
fn slice_frame_num(au: &[u8], sps: &Sps) -> (u32, bool) {
    let nal = annexb_nals(au)
        .find(|n| n.first().is_some_and(|b| matches!(b & 0x1f, 1 | 5)))
        .expect("access unit without a slice");
    let reference = (nal[0] >> 5) & 3 != 0;
    let data = rbsp(&nal[1..]);
    let mut b = Bits { data: &data, pos: 0 };
    b.ue(); // first_mb_in_slice
    b.ue(); // slice_type
    b.ue(); // pic_parameter_set_id
    (b.u(sps.log2_max_frame_num), reference)
}

/// Assert every received picture continues from the one before it, starting at
/// an IDR, and return the last SPS seen.
fn assert_decodable(frames: &[Received]) -> Sps {
    let mut sps: Option<Sps> = None;
    let mut prev: Option<(u32, bool)> = None;
    for (i, r) in frames.iter().enumerate() {
        let au = &r.frame.data;
        if let Some(nal) = annexb_nals(au).find(|n| n.first().is_some_and(|b| b & 0x1f == 7)) {
            sps = Some(parse_sps(nal));
        }
        let sps = sps.expect("the stream must start with an SPS");
        let (frame_num, reference) = slice_frame_num(au, &sps);
        assert_eq!(r.frame.keyframe, h264_is_keyframe(au), "frame {i}: keyframe flag");
        if r.frame.keyframe {
            assert_eq!(frame_num, 0, "frame {i}: an IDR has frame_num 0");
        } else {
            let (p_num, p_ref) = prev.expect("the first frame must be a keyframe");
            let expected = (p_num + u32::from(p_ref)) % (1 << sps.log2_max_frame_num);
            assert_eq!(
                frame_num, expected,
                "frame {i}: a reference picture is missing (frame_num gap)"
            );
        }
        prev = Some((frame_num, reference));
    }
    sps.expect("no frames")
}

fn assert_timestamps_increase(frames: &[Received]) {
    for pair in frames.windows(2) {
        let step = pair[1].frame.ts.wrapping_sub(pair[0].frame.ts);
        assert!(step > 0 && step < 90_000 * 2, "ts step {step} ticks");
    }
}

// ----------------------------------------------------------------- the tests

#[tokio::test]
async fn start_rejects_malformed_ids_without_touching_the_gpu() {
    let (tx, _rx) = mpsc::channel(1);
    let err = ScreenShare::start("screen:7", ShareMode::Text, tx).await.unwrap_err();
    assert_eq!(err, Error::InvalidSource("screen:7".into()));
}

#[test]
#[ignore = "needs a GPU with a hardware H.264 encoder"]
fn hardware_encoder_is_available_here() {
    init_tracing();
    let t = Instant::now();
    assert!(hardware_encoder_available());
    let first = t.elapsed();
    let t = Instant::now();
    assert!(hardware_encoder_available());
    println!("probe took {first:?}, cached call {:?}", t.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an interactive desktop"]
async fn lists_displays_first_then_windows() {
    init_tracing();
    let list = sources().await.unwrap();
    for s in &list {
        println!(
            "{:<22} {:<8} {:>5}x{:<5} {}",
            s.id,
            s.kind.as_str(),
            s.width,
            s.height,
            s.name
        );
    }
    assert!(!list.is_empty());
    assert_eq!(list[0].kind, SourceKind::Display);
    assert_eq!(list[0].id, "display:0", "the primary display is first");
    let first_window = list
        .iter()
        .position(|s| s.kind == SourceKind::Window)
        .unwrap_or(list.len());
    assert!(
        list[first_window..].iter().all(|s| s.kind == SourceKind::Window),
        "all displays come before all windows"
    );
    for s in &list {
        assert!(s.width > 0 && s.height > 0, "{s:?}");
        assert!(!s.name.trim().is_empty(), "{s:?}");
        if s.kind == SourceKind::Window {
            assert!(s.width >= MIN_WINDOW_SIDE && s.height >= MIN_WINDOW_SIDE, "{s:?}");
            assert!(s.id.starts_with("window:"), "{s:?}");
            assert_ne!(s.name, "Program Manager");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an interactive desktop and a hardware H.264 encoder"]
async fn shares_the_primary_display() {
    init_tracing();
    let (tx, mut rx) = mpsc::channel(256);
    let t0 = Instant::now();
    let share = ScreenShare::start("display:0", ShareMode::Text, tx).await.unwrap();
    let start_took = t0.elapsed();
    assert_eq!(share.end_reason(), None);

    let mut got = Vec::new();
    collect(&mut rx, Duration::from_millis(1500), &mut got).await;
    let requested_at = Instant::now();
    share.request_keyframe();
    collect(&mut rx, Duration::from_millis(1500), &mut got).await;
    let stats = share.stats();

    let ended = share.ended();
    let stop_at = Instant::now();
    share.stop();
    let stop_took = stop_at.elapsed();
    assert_eq!(*ended.borrow(), Some(EndReason::Stopped));
    while let Ok(frame) = rx.try_recv() {
        got.push(Received {
            at: Instant::now(),
            frame,
        });
    }

    let first = &got.first().expect("no frames in 3 s").frame;
    let codec = h264_codec_string(&first.data);
    let bytes: usize = got.iter().map(|r| r.frame.data.len()).sum();
    let keys = got.iter().filter(|r| r.frame.keyframe).count();
    let largest = got.iter().map(|r| r.frame.data.len()).max().unwrap_or(0);
    println!(
        "start {start_took:?}, stop {stop_took:?}, {} frames, {keys} keyframes, {:.0} kbps, largest {largest} B, \
         codec {codec:?}, first NALs {:?}, stats {stats:?}",
        got.len(),
        bytes as f64 * 8.0 / 3.0 / 1000.0,
        nal_types(&first.data),
    );

    assert!(first.keyframe, "the first frame must be a keyframe");
    let types = nal_types(&first.data);
    assert!(
        types.contains(&7) && types.contains(&8) && types.contains(&5),
        "SPS+PPS+IDR: {types:?}"
    );
    assert!(
        codec
            .as_deref()
            .is_some_and(|c| c.starts_with("avc1.") && c.len() == 11),
        "{codec:?}"
    );
    assert!(
        got.len() >= 20,
        "15 fps for 3 s should give ~45 frames, got {}",
        got.len()
    );
    assert!(got.len() <= 50, "text mode is capped at 15 fps, got {}", got.len());
    assert!(
        got.iter()
            .all(|r| !r.frame.data.is_empty() && r.frame.data.len() < 2_000_000)
    );
    assert!(
        (bytes as f64 * 8.0 / 3.0) < 2.0 * 2_500_000.0,
        "bitrate far above the cap"
    );
    assert_timestamps_increase(&got);
    let sps = assert_decodable(&got);
    assert_eq!(
        (sps.width, sps.height),
        (stats.width, stats.height),
        "SPS matches the encoder size"
    );
    assert!(
        stats.width <= 1920 && stats.width % 2 == 0 && stats.height % 2 == 0,
        "{stats:?}"
    );
    assert!(stats.frames_sent as usize >= got.len().saturating_sub(1), "{stats:?}");
    assert_eq!(stats.frames_dropped, 0, "nothing should drop with a 256-frame channel");
    assert!(stats.fps > 8.0 && stats.fps < 17.0, "{stats:?}");
    assert_eq!(stats.target_bitrate_bps, ShareMode::Text.target_bitrate_bps());

    let forced = got
        .iter()
        .find(|r| r.at >= requested_at && r.frame.keyframe)
        .map(|r| r.at - requested_at);
    println!("keyframe after request: {forced:?}");
    assert!(forced.is_some_and(|d| d <= Duration::from_secs(1)), "{forced:?}");
    assert!(stop_took < Duration::from_millis(500), "stop took {stop_took:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an interactive desktop and a hardware H.264 encoder"]
async fn a_full_channel_drops_to_the_next_keyframe() {
    init_tracing();
    // One slot and a slow reader: most frames are dropped.
    let (tx, mut rx) = mpsc::channel(1);
    let share = ScreenShare::start("display:0", ShareMode::Motion, tx).await.unwrap();
    let mut got = Vec::new();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_millis(200)).await;
        while let Ok(frame) = rx.try_recv() {
            got.push(Received {
                at: Instant::now(),
                frame,
            });
        }
    }
    let stats = share.stats();
    share.stop();
    let keys = got.iter().filter(|r| r.frame.keyframe).count();
    println!("{} frames received ({keys} keyframes), stats {stats:?}", got.len());
    assert!(stats.frames_dropped > 0, "{stats:?}");
    assert!(keys >= 3, "drops force keyframes (at most one per 500 ms): {keys}");
    assert!(keys <= 8, "forced keyframes are rate limited: {keys}");
    assert_decodable(&got);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an interactive desktop and a hardware H.264 encoder"]
async fn low_bitrate_switches_to_the_720p_tier_and_back() {
    init_tracing();
    let (tx, mut rx) = mpsc::channel(256);
    let share = ScreenShare::start("display:0", ShareMode::Motion, tx).await.unwrap();
    let mut got = Vec::new();
    collect(&mut rx, Duration::from_millis(800), &mut got).await;
    let normal = share.stats();

    share.set_bitrate(100_000);
    collect(&mut rx, Duration::from_millis(2600), &mut got).await;
    let low = share.stats();

    share.set_bitrate(10_000_000);
    collect(&mut rx, Duration::from_millis(1000), &mut got).await;
    let back = share.stats();
    share.stop();

    println!("normal {normal:?}\nlow {low:?}\nback {back:?}");
    assert_eq!(low.target_bitrate_bps, 300_000, "clamped to the floor");
    assert_eq!(back.target_bitrate_bps, 4_500_000, "clamped to the motion cap");
    assert!(low.width <= 1280 && low.height <= 1280, "{low:?}");
    assert!(u64::from(low.width) * u64::from(low.height) <= 1280 * 720, "{low:?}");
    assert!(low.fps <= 11.0, "the low tier runs at 10 fps: {low:?}");
    assert_eq!((back.width, back.height), (normal.width, normal.height));
    let sps = assert_decodable(&got);
    assert_eq!((sps.width, sps.height), (back.width, back.height));
}

#[cfg(windows)]
mod window {
    use std::ffi::c_void;

    use moq_video::windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use moq_video::windows::Win32::System::Threading::GetCurrentThreadId;
    use moq_video::windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, MSG, PostMessageW, PostThreadMessageW, SWP_NOMOVE,
        SWP_NOZORDER, SetWindowPos, SetWindowTextW, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_QUIT,
        WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };
    use moq_video::windows::core::PCWSTR;

    /// A visible top-level window with its own message loop thread.
    pub struct TestWindow {
        pub hwnd: usize,
        thread_id: u32,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl TestWindow {
        pub fn open(title: &str) -> TestWindow {
            let (tx, rx) = std::sync::mpsc::channel();
            let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
            let thread = std::thread::spawn(move || {
                let class: Vec<u16> = "STATIC".encode_utf16().chain([0]).collect();
                // SAFETY: plain Win32 window creation and a message loop on
                // this thread; the strings outlive the calls.
                unsafe {
                    let hwnd = CreateWindowExW(
                        WINDOW_EX_STYLE::default(),
                        PCWSTR(class.as_ptr()),
                        PCWSTR(title.as_ptr()),
                        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                        120,
                        120,
                        641,
                        481,
                        None,
                        None,
                        None,
                        None,
                    )
                    .expect("create test window");
                    tx.send((hwnd.0 as usize, GetCurrentThreadId())).expect("report window");
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            });
            let (hwnd, thread_id) = rx.recv().expect("window thread");
            TestWindow {
                hwnd,
                thread_id,
                thread: Some(thread),
            }
        }

        fn handle(&self) -> HWND {
            HWND(self.hwnd as *mut c_void)
        }

        /// Change the window text, which the STATIC class repaints.
        pub fn set_text(&self, text: &str) {
            let text: Vec<u16> = text.encode_utf16().chain([0]).collect();
            // SAFETY: a window we created; the string outlives the call.
            unsafe { SetWindowTextW(self.handle(), PCWSTR(text.as_ptr())).expect("set window text") };
        }

        /// Resize the window (outer size, pixels).
        pub fn resize(&self, width: i32, height: i32) {
            // SAFETY: a window we created.
            unsafe {
                SetWindowPos(self.handle(), None, 0, 0, width, height, SWP_NOMOVE | SWP_NOZORDER)
                    .expect("resize window");
            }
        }

        /// Ask the window to close (it is destroyed by DefWindowProc).
        pub fn close(&self) {
            // SAFETY: posting a message to a window handle we created.
            unsafe {
                PostMessageW(Some(self.handle()), WM_CLOSE, WPARAM(0), LPARAM(0)).expect("post WM_CLOSE");
            }
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            // SAFETY: ends the message loop of the thread we started.
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an interactive desktop and a hardware H.264 encoder; opens a window"]
async fn a_window_share_survives_resizes_and_ends_when_it_closes() {
    init_tracing();
    let window = window::TestWindow::open("tincan-screen live test window");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let id = format!("window:{}", window.hwnd);
    let listed = sources().await.unwrap();
    assert!(
        !listed.iter().any(|s| s.id == id),
        "windows of this process (Tincan itself) are not offered"
    );

    // Not listed, but still shareable by id.
    let (tx, mut rx) = mpsc::channel(256);
    let share = ScreenShare::start(&id, ShareMode::Text, tx).await.unwrap();
    assert_eq!(share.source_kind(), SourceKind::Window);
    let mut got = Vec::new();
    collect(&mut rx, Duration::from_millis(1000), &mut got).await;
    let before = share.stats();
    assert!(!got.is_empty());
    assert!(got[0].frame.keyframe);
    assert_eq!((before.width % 2, before.height % 2), (0, 0), "{before:?}");

    // A resize ends the capture stream; the share reopens it and rebuilds the
    // encoder at the new size without the consumer noticing beyond a keyframe.
    window.resize(803, 603);
    collect(&mut rx, Duration::from_millis(1500), &mut got).await;
    let after = share.stats();
    println!(
        "window share: {} frames, before {before:?}, after resize {after:?}",
        got.len()
    );
    assert_eq!(share.end_reason(), None, "a resize does not end the share");
    assert_ne!(
        (after.width, after.height),
        (before.width, before.height),
        "rebuilt at the new size"
    );
    assert_eq!((after.width % 2, after.height % 2), (0, 0), "{after:?}");
    assert_timestamps_increase(&got);
    let sps = assert_decodable(&got);
    assert_eq!((sps.width, sps.height), (after.width, after.height));

    let mut ended = share.ended();
    let closed_at = Instant::now();
    window.close();
    let reason = tokio::time::timeout(Duration::from_secs(3), ended.wait_for(Option::is_some))
        .await
        .expect("share did not end within 3 s of the window closing")
        .expect("end signal")
        .clone();
    println!("ended after {:?}: {reason:?}", closed_at.elapsed());
    assert!(matches!(reason, Some(EndReason::SourceClosed(_))), "{reason:?}");
    let stop_at = Instant::now();
    share.stop();
    assert!(
        stop_at.elapsed() < Duration::from_millis(200),
        "stop after end is immediate"
    );
    drop(window);
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an interactive desktop and a hardware H.264 encoder; opens a window"]
async fn changing_content_reaches_the_motion_frame_cap() {
    init_tracing();
    let window = std::sync::Arc::new(window::TestWindow::open("tincan-screen motion test"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let id = format!("window:{}", window.hwnd);
    let (tx, mut rx) = mpsc::channel(256);
    let share = ScreenShare::start(&id, ShareMode::Motion, tx).await.unwrap();

    // Repaint the window continuously from another thread.
    let painting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let painter = std::thread::spawn({
        let window = window.clone();
        let painting = painting.clone();
        move || {
            let mut n = 0u64;
            while painting.load(std::sync::atomic::Ordering::Relaxed) {
                n += 1;
                window.set_text(&format!("frame {n} {}", "#".repeat((n % 40) as usize)));
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    });
    let mut got = Vec::new();
    collect(&mut rx, Duration::from_millis(3000), &mut got).await;
    let stats = share.stats();
    painting.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = painter.join();
    share.stop();
    println!("motion content: {} frames in 3 s, stats {stats:?}", got.len());
    assert!(stats.fps >= 24.0 && stats.fps <= 31.0, "{stats:?}");
    assert!(got.len() <= 95, "capped at 30 fps: {}", got.len());
    assert_decodable(&got);
}

/// Thread count, handle count and private bytes of this process, read through
/// `Get-Process` in a child PowerShell (std exposes none of them).
fn process_stats() -> (u64, u64, u64) {
    let pid = std::process::id();
    let script =
        format!("$p = Get-Process -Id {pid}; \"$($p.Threads.Count) $($p.HandleCount) $($p.PrivateMemorySize64)\"");
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .expect("run powershell");
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Vec<u64> = text.split_whitespace().filter_map(|s| s.parse().ok()).collect();
    assert_eq!(v.len(), 3, "unexpected Get-Process output: {text:?}");
    (v[0], v[1], v[2])
}

async fn short_share(id: &str, mode: ShareMode, millis: u64) -> usize {
    let (tx, mut rx) = mpsc::channel(256);
    let share = ScreenShare::start(id, mode, tx).await.unwrap();
    let mut got = Vec::new();
    collect(&mut rx, Duration::from_millis(millis), &mut got).await;
    share.stop();
    got.len()
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "soak: 20 share start/stop cycles; needs an interactive desktop and a hardware H.264 encoder"]
async fn repeated_shares_do_not_leak() {
    init_tracing();
    // Warm up process-wide state (probe, MTA pin, moq's scaler cache, WinRT factories).
    short_share("display:0", ShareMode::Text, 300).await;
    short_share("display:0", ShareMode::Motion, 300).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let before = process_stats();

    let second = if sources().await.unwrap().iter().any(|s| s.id == "display:1") {
        "display:1"
    } else {
        "display:0"
    };
    let window = window::TestWindow::open("tincan-screen soak window");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let window_id = format!("window:{}", window.hwnd);
    // TINCAN_SCREEN_SOAK_CYCLES overrides the cycle count for longer soaks.
    let cycles: i32 = std::env::var("TINCAN_SCREEN_SOAK_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let mut frames = 0;
    for cycle in 0..cycles {
        frames += match cycle % 4 {
            0 => short_share("display:0", ShareMode::Text, 300).await,
            1 => short_share("display:0", ShareMode::Motion, 300).await,
            2 => short_share(second, ShareMode::Motion, 300).await,
            _ => {
                // A window share with a resize in the middle (capture reopen).
                let (tx, mut rx) = mpsc::channel(256);
                let share = ScreenShare::start(&window_id, ShareMode::Text, tx).await.unwrap();
                let mut got = Vec::new();
                collect(&mut rx, Duration::from_millis(300), &mut got).await;
                window.resize(640 + 2 * cycle, 480 + 2 * cycle);
                collect(&mut rx, Duration::from_millis(600), &mut got).await;
                assert_eq!(share.end_reason(), None);
                share.stop();
                got.len()
            }
        };
    }
    drop(window);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = process_stats();
    println!(
        "soak: {frames} frames over {cycles} shares; threads/handles/private bytes before {before:?} after {after:?}"
    );
    assert!(frames > cycles as usize * 3, "every share produced frames: {frames}");
    assert!(after.0 <= before.0 + 4, "thread growth: {before:?} -> {after:?}");
    // Known and small: each hardware encoder session leaks about 4 handles
    // inside Media Foundation (moq-video 0.1.23 never shuts the MFT down) and
    // each capture session about 2; 25 encoder sessions here (20 shares plus 5
    // resize rebuilds). Threads and memory must not grow at all.
    let allowed = 10 * u64::try_from(cycles).unwrap_or(0) + 50;
    assert!(after.1 <= before.1 + allowed, "handle growth: {before:?} -> {after:?}");
    assert!(
        after.2 <= before.2 + 96 * 1024 * 1024,
        "private memory growth: {before:?} -> {after:?}"
    );
}
