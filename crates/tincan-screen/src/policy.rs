//! Pure rate, size and timing policy for a screen share.
//!
//! Nothing here touches the GPU, the desktop or a clock it does not get handed,
//! so every rule is unit-tested on any host. The capture/encode loop in
//! `share.rs` only wires these pieces to real frames.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Bitrate floor shared by every mode, in bits per second.
pub const MIN_BITRATE_BPS: u32 = 300_000;

/// A target below this drops the share to the low tier (720p class, 10 fps).
pub const LOW_TIER_ENTER_BPS: u32 = 600_000;

/// A target at or above this returns the share from the low tier to the normal
/// one. Higher than [`LOW_TIER_ENTER_BPS`] so a controller hovering around the
/// threshold does not rebuild the encoder (and force a keyframe) every step.
pub const LOW_TIER_EXIT_BPS: u32 = 750_000;

/// Keyframe interval in wall time. Media Foundation counts its GOP in frames,
/// which stretches whenever capture pauses, so the loop also cuts on this clock.
pub const GOP: Duration = Duration::from_secs(4);

/// Minimum spacing between keyframes forced by requests or drops.
pub const KEYFRAME_MIN_INTERVAL: Duration = Duration::from_millis(500);

/// Widest picture the encoder is fed; wider sources are downscaled.
pub const MAX_ENCODE_WIDTH: u32 = 1920;

/// Frame rate of the low tier.
pub const LOW_TIER_FPS: u32 = 10;

/// What the share is optimised for. Chosen by the sharer in the UI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ShareMode {
    /// Documents, code, slides (the default): up to 15 fps, 1.2 Mbps target,
    /// 2.5 Mbps cap, up to about 2.3 Mpx (1920x1200) without scaling.
    #[default]
    Text,
    /// Video and games: up to 30 fps, 3 Mbps target, 4.5 Mbps cap, up to
    /// 1920x1080 worth of pixels.
    Motion,
}

impl ShareMode {
    /// Highest frame rate this mode encodes.
    pub const fn max_fps(self) -> u32 {
        match self {
            ShareMode::Text => 15,
            ShareMode::Motion => 30,
        }
    }

    /// Bitrate a share starts at, in bits per second.
    pub const fn target_bitrate_bps(self) -> u32 {
        match self {
            ShareMode::Text => 1_200_000,
            ShareMode::Motion => 3_000_000,
        }
    }

    /// Highest bitrate [`ScreenShare::set_bitrate`](crate::ScreenShare::set_bitrate)
    /// accepts for this mode, in bits per second.
    pub const fn max_bitrate_bps(self) -> u32 {
        match self {
            ShareMode::Text => 2_500_000,
            ShareMode::Motion => 4_500_000,
        }
    }

    /// Clamp a requested bitrate to `MIN_BITRATE_BPS ..= max_bitrate_bps()`.
    pub const fn clamp_bitrate(self, bps: u32) -> u32 {
        let max = self.max_bitrate_bps();
        if bps < MIN_BITRATE_BPS {
            MIN_BITRATE_BPS
        } else if bps > max {
            max
        } else {
            bps
        }
    }

    /// Largest picture area (pixels) encoded in the normal tier.
    pub const fn max_pixels(self) -> u64 {
        match self {
            ShareMode::Text => 1920 * 1200,
            ShareMode::Motion => 1920 * 1080,
        }
    }

    /// The UI/API string for this mode: `"text"` or `"motion"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            ShareMode::Text => "text",
            ShareMode::Motion => "motion",
        }
    }

    /// Parse the UI/API string (`"text"` or `"motion"`, case-insensitive).
    pub fn parse(value: &str) -> Option<ShareMode> {
        if value.eq_ignore_ascii_case("text") {
            Some(ShareMode::Text)
        } else if value.eq_ignore_ascii_case("motion") {
            Some(ShareMode::Motion)
        } else {
            None
        }
    }
}

/// Quality tier inside a mode. The low tier trades resolution and frame rate
/// for legibility when the network allows less than [`LOW_TIER_ENTER_BPS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    Normal,
    Low,
}

impl Tier {
    /// The tier a target bitrate selects, with hysteresis around the threshold.
    pub(crate) fn next(self, bps: u32) -> Tier {
        if bps < LOW_TIER_ENTER_BPS {
            Tier::Low
        } else if bps >= LOW_TIER_EXIT_BPS {
            Tier::Normal
        } else {
            self
        }
    }
}

/// Encoder geometry and rate limits for one mode and tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_pixels: u64,
    pub fps: u32,
}

impl Limits {
    pub(crate) fn of(mode: ShareMode, tier: Tier) -> Limits {
        match tier {
            Tier::Normal => Limits {
                max_width: MAX_ENCODE_WIDTH,
                // Tall windows: hardware encoders top out around 4096, and a
                // portrait 1440p monitor (1440x2560) is the tallest sane source.
                max_height: 2560,
                max_pixels: mode.max_pixels(),
                fps: mode.max_fps(),
            },
            Tier::Low => Limits {
                max_width: 1280,
                max_height: 1280,
                max_pixels: 1280 * 720,
                fps: LOW_TIER_FPS.min(mode.max_fps()),
            },
        }
    }

    /// The encoded size for a `width` x `height` source under these limits.
    pub(crate) fn fit(&self, width: u32, height: u32) -> (u32, u32) {
        fit(width, height, self.max_width, self.max_height, self.max_pixels)
    }
}

/// Scale `width` x `height` down (never up) to fit `max_width`, `max_height`
/// and `max_pixels`, keeping the aspect ratio. Both results are even and at
/// least 2, as 4:2:0 chroma requires.
pub(crate) fn fit(width: u32, height: u32, max_width: u32, max_height: u32, max_pixels: u64) -> (u32, u32) {
    let (w, h) = (width.max(2), height.max(2));
    let (max_w, max_h) = (max_width.max(2) & !1, max_height.max(2) & !1);
    let max_px = max_pixels.max(4);
    let wf = f64::from(w);
    let hf = f64::from(h);
    let mut scale = 1.0f64;
    scale = scale.min(f64::from(max_w) / wf);
    scale = scale.min(f64::from(max_h) / hf);
    let pixels = wf * hf;
    if pixels > max_px as f64 {
        scale = scale.min((max_px as f64 / pixels).sqrt());
    }
    let (mut tw, mut th) = if scale >= 1.0 {
        (w & !1, h & !1)
    } else {
        // The epsilon keeps an exact ratio (3840 * 0.5) from flooring to 1918.
        let tw = ((wf * scale + 1e-6).floor() as u32) & !1;
        let th = ((hf * scale + 1e-6).floor() as u32) & !1;
        (tw.max(2), th.max(2))
    };
    tw = tw.min(max_w);
    th = th.min(max_h);
    while u64::from(tw) * u64::from(th) > max_px && th > 2 {
        th -= 2;
    }
    (tw, th)
}

/// Caps the frame rate over capture timestamps without halving it under jitter.
///
/// A deadline accumulator: each admitted frame moves the next deadline one
/// interval along the grid, and a frame is admitted once it is within a quarter
/// interval of that deadline. Frames at the cap rate with jitter all pass,
/// frames at twice the cap pass every other one, and after a pause (variable
/// frame rate capture) the grid restarts at the next frame.
#[derive(Clone, Debug)]
pub(crate) struct RateLimiter {
    interval_us: u64,
    slack_us: u64,
    next_us: Option<u64>,
}

impl RateLimiter {
    pub(crate) fn new(fps: u32) -> Self {
        let interval_us = 1_000_000 / u64::from(fps.max(1));
        Self {
            interval_us,
            slack_us: interval_us / 4,
            next_us: None,
        }
    }

    /// Change the cap. The next frame is admitted and starts a new grid.
    pub(crate) fn set_fps(&mut self, fps: u32) {
        *self = Self::new(fps);
    }

    /// Whether a frame captured at `t_us` (monotonic microseconds) is encoded.
    pub(crate) fn admit(&mut self, t_us: u64) -> bool {
        let Some(next) = self.next_us else {
            self.next_us = Some(t_us.saturating_add(self.interval_us));
            return true;
        };
        if t_us.saturating_add(self.slack_us) < next {
            return false;
        }
        // Late by a whole interval or more: the source paused, restart the grid.
        let base = if t_us >= next.saturating_add(self.interval_us) {
            t_us
        } else {
            next
        };
        self.next_us = Some(base.saturating_add(self.interval_us));
        true
    }
}

/// Coalesces keyframe requests to at most one forced keyframe per
/// [`KEYFRAME_MIN_INTERVAL`], firing deferred requests on the trailing edge.
#[derive(Clone, Debug)]
pub(crate) struct KeyframeGate {
    min_interval: Duration,
    pending: bool,
    last: Option<Instant>,
}

impl KeyframeGate {
    pub(crate) fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            pending: false,
            last: None,
        }
    }

    /// Someone needs a keyframe (a viewer, the host, or a local drop).
    pub(crate) fn request(&mut self) {
        self.pending = true;
    }

    /// Whether a request is waiting for its window.
    #[cfg(test)]
    pub(crate) fn is_pending(&self) -> bool {
        self.pending
    }

    /// Called before encoding a frame at `now`: true means cut this frame.
    pub(crate) fn poll(&mut self, now: Instant) -> bool {
        if !self.pending {
            return false;
        }
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < self.min_interval)
        {
            return false;
        }
        self.pending = false;
        self.last = Some(now);
        true
    }

    /// A keyframe went out at `now` (forced or on the encoder's own schedule).
    /// It satisfies every request made before it and starts a new window.
    pub(crate) fn on_keyframe(&mut self, now: Instant) {
        self.pending = false;
        self.last = Some(now);
    }
}

/// Sliding-window meter of sent frames for [`ShareStats`](crate::ShareStats).
#[derive(Debug)]
pub(crate) struct Meter {
    window: Duration,
    start: Instant,
    samples: VecDeque<(Instant, u32)>,
}

impl Meter {
    /// Shortest span the rates are averaged over, so the first frames of a share
    /// do not report thousands of frames per second.
    const MIN_SPAN: Duration = Duration::from_millis(500);

    pub(crate) fn new(window: Duration, start: Instant) -> Self {
        Self {
            window,
            start,
            samples: VecDeque::with_capacity(64),
        }
    }

    /// One frame of `bytes` was sent at `now`.
    pub(crate) fn record(&mut self, now: Instant, bytes: usize) {
        self.prune(now);
        self.samples.push_back((now, u32::try_from(bytes).unwrap_or(u32::MAX)));
    }

    /// Frames per second and bits per second over the window ending at `now`.
    pub(crate) fn rates(&mut self, now: Instant) -> (f32, u32) {
        self.prune(now);
        let span = now
            .saturating_duration_since(self.start)
            .min(self.window)
            .max(Self::MIN_SPAN)
            .as_secs_f64();
        let frames = self.samples.len() as f64;
        let bits: f64 = self.samples.iter().map(|(_, b)| f64::from(*b) * 8.0).sum();
        ((frames / span) as f32, (bits / span).min(f64::from(u32::MAX)) as u32)
    }

    fn prune(&mut self, now: Instant) {
        while let Some((t, _)) = self.samples.front() {
            if now.saturating_duration_since(*t) > self.window {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }
}

/// One monotonic microsecond timeline across capture reopens.
///
/// Each `capture::Stream` stamps frames on a private timeline that starts near
/// zero when it opens, so a reopen (window resize, display mode change) would
/// rewind timestamps. Every stream is offset by when it was opened relative to
/// the share's start, and the result is forced strictly increasing.
#[derive(Clone, Debug)]
pub(crate) struct Timeline {
    epoch: Instant,
    offset_us: u64,
    last_us: Option<u64>,
}

impl Timeline {
    pub(crate) fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            offset_us: 0,
            last_us: None,
        }
    }

    /// A new capture stream was opened at `opened` (just before `capture::open`).
    pub(crate) fn rebase(&mut self, opened: Instant) {
        let micros = opened.saturating_duration_since(self.epoch).as_micros();
        self.offset_us = u64::try_from(micros).unwrap_or(u64::MAX);
    }

    /// Map a stream-relative timestamp to the share timeline.
    pub(crate) fn map(&mut self, stream_us: u64) -> u64 {
        let mut t = self.offset_us.saturating_add(stream_us);
        if let Some(last) = self.last_us
            && t <= last
        {
            t = last.saturating_add(1);
        }
        self.last_us = Some(t);
        t
    }
}

/// Microseconds to a 90 kHz RTP-style timestamp, wrapping at 2^32 (about 13 h).
pub(crate) fn to_90khz(us: u64) -> u32 {
    // Truncating the u128 keeps the low 32 bits, which is the wrap we want.
    ((u128::from(us) * 9) / 100) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_clamps_to_floor_and_mode_cap() {
        assert_eq!(ShareMode::Text.clamp_bitrate(0), MIN_BITRATE_BPS);
        assert_eq!(ShareMode::Text.clamp_bitrate(299_999), 300_000);
        assert_eq!(ShareMode::Text.clamp_bitrate(1_000_000), 1_000_000);
        assert_eq!(ShareMode::Text.clamp_bitrate(9_000_000), 2_500_000);
        assert_eq!(ShareMode::Motion.clamp_bitrate(9_000_000), 4_500_000);
        assert_eq!(ShareMode::Motion.clamp_bitrate(u32::MAX), 4_500_000);
        assert_eq!(ShareMode::Motion.clamp_bitrate(4_500_000), 4_500_000);
        for mode in [ShareMode::Text, ShareMode::Motion] {
            let target = mode.target_bitrate_bps();
            assert_eq!(
                mode.clamp_bitrate(target),
                target,
                "the default target is inside the range"
            );
        }
    }

    #[test]
    fn mode_strings_round_trip() {
        for mode in [ShareMode::Text, ShareMode::Motion] {
            assert_eq!(ShareMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ShareMode::parse("MOTION"), Some(ShareMode::Motion));
        assert_eq!(ShareMode::parse("video"), None);
        assert_eq!(ShareMode::default(), ShareMode::Text);
        assert_eq!(ShareMode::Text.max_fps(), 15);
        assert_eq!(ShareMode::Motion.max_fps(), 30);
    }

    #[test]
    fn tier_has_hysteresis() {
        let t = Tier::Normal;
        assert_eq!(t.next(1_200_000), Tier::Normal);
        assert_eq!(t.next(650_000), Tier::Normal, "between thresholds keeps the tier");
        assert_eq!(t.next(599_999), Tier::Low);
        let t = Tier::Low;
        assert_eq!(t.next(700_000), Tier::Low, "between thresholds keeps the tier");
        assert_eq!(t.next(750_000), Tier::Normal);
    }

    #[test]
    fn sources_at_or_below_the_limits_are_untouched() {
        let text = Limits::of(ShareMode::Text, Tier::Normal);
        assert_eq!(text.fit(1920, 1200), (1920, 1200));
        assert_eq!(text.fit(1920, 1080), (1920, 1080));
        assert_eq!(text.fit(1280, 720), (1280, 720));
        assert_eq!(
            text.fit(1080, 1920),
            (1080, 1920),
            "portrait 1080p fits the pixel budget"
        );
        assert_eq!(text.fit(801, 601), (800, 600), "odd sizes are cropped to even");
    }

    #[test]
    fn wide_sources_downscale_to_1920_keeping_aspect() {
        let text = Limits::of(ShareMode::Text, Tier::Normal);
        assert_eq!(text.fit(3840, 2160), (1920, 1080));
        assert_eq!(text.fit(2560, 1440), (1920, 1080));
        assert_eq!(text.fit(2560, 1600), (1920, 1200));
        let (w, h) = text.fit(3440, 1440);
        assert_eq!(w, 1920);
        assert_eq!(h % 2, 0);
        let aspect = |w: u32, h: u32| f64::from(w) / f64::from(h);
        assert!((aspect(w, h) - aspect(3440, 1440)).abs() < 0.01, "{w}x{h}");
        let (w, h) = text.fit(5120, 1440);
        assert_eq!((w, h % 2), (1920, 0));
        assert!((aspect(w, h) - aspect(5120, 1440)).abs() < 0.02, "{w}x{h}");
    }

    #[test]
    fn motion_mode_caps_area_at_1080p() {
        let motion = Limits::of(ShareMode::Motion, Tier::Normal);
        assert_eq!(motion.fit(1920, 1080), (1920, 1080));
        let (w, h) = motion.fit(1920, 1200);
        assert!(u64::from(w) * u64::from(h) <= 1920 * 1080, "{w}x{h}");
        assert_eq!((w % 2, h % 2), (0, 0));
        assert!((f64::from(w) / f64::from(h) - 1.6).abs() < 0.01, "{w}x{h}");
        assert_eq!(motion.fps, 30);
    }

    #[test]
    fn low_tier_is_720p_class_at_10fps() {
        for mode in [ShareMode::Text, ShareMode::Motion] {
            let low = Limits::of(mode, Tier::Low);
            assert_eq!(low.fps, 10);
            assert_eq!(low.fit(1920, 1080), (1280, 720));
            assert_eq!(low.fit(3840, 2160), (1280, 720));
            let (w, h) = low.fit(1080, 1920);
            assert!(u64::from(w) * u64::from(h) <= 1280 * 720 && h <= 1280, "{w}x{h}");
        }
    }

    #[test]
    fn fit_never_upscales_and_stays_even_and_inside_every_limit() {
        for (w, h) in [
            (2, 2),
            (3, 3),
            (100, 7000),
            (7000, 100),
            (1366, 768),
            (1919, 1079),
            (4096, 4096),
            (7680, 4320),
            (65_535, 65_535),
        ] {
            for limits in [
                Limits::of(ShareMode::Text, Tier::Normal),
                Limits::of(ShareMode::Motion, Tier::Normal),
                Limits::of(ShareMode::Text, Tier::Low),
            ] {
                let (tw, th) = limits.fit(w, h);
                assert!(tw >= 2 && th >= 2, "{w}x{h} -> {tw}x{th}");
                assert_eq!((tw % 2, th % 2), (0, 0), "{w}x{h} -> {tw}x{th}");
                assert!(tw <= w.max(2) && th <= h.max(2), "never upscale: {w}x{h} -> {tw}x{th}");
                assert!(
                    tw <= limits.max_width && th <= limits.max_height,
                    "{w}x{h} -> {tw}x{th}"
                );
                assert!(
                    u64::from(tw) * u64::from(th) <= limits.max_pixels,
                    "{w}x{h} -> {tw}x{th}"
                );
            }
        }
    }

    /// Feed frames at `in_fps` with deterministic jitter for `secs`; return the
    /// number admitted.
    fn admitted(cap: u32, in_fps: f64, jitter_us: i64, secs: u64) -> usize {
        let mut limiter = RateLimiter::new(cap);
        let period = 1_000_000.0 / in_fps;
        let frames = (secs as f64 * in_fps) as u64;
        (0..frames)
            .filter(|i| {
                // Alternate early/late arrivals around the nominal grid.
                let j = if i % 2 == 0 { jitter_us } else { -jitter_us };
                let t = (*i as f64 * period) as i64 + 1_000_000 + j;
                limiter.admit(t as u64)
            })
            .count()
    }

    #[test]
    fn limiter_passes_input_at_the_cap_even_with_jitter() {
        // 15 fps in, 15 fps cap, +-5 ms jitter: nothing may be dropped.
        assert_eq!(admitted(15, 15.0, 5_000, 10), 150);
        // Slightly slower than nominal (WGC delivers at >= its interval).
        let n = admitted(15, 14.8, 3_000, 10);
        assert_eq!(n, 148);
        assert_eq!(admitted(30, 30.0, 4_000, 10), 300);
    }

    #[test]
    fn limiter_caps_faster_input() {
        let n = admitted(15, 30.0, 2_000, 10);
        assert!((149..=151).contains(&n), "30 fps into a 15 fps cap: {n}");
        let n = admitted(15, 60.0, 1_000, 10);
        assert!((149..=151).contains(&n), "60 fps into a 15 fps cap: {n}");
        // 15 fps into the 10 fps low tier keeps two of every three frames.
        let n = admitted(10, 15.0, 0, 9);
        assert!((89..=91).contains(&n), "15 fps into a 10 fps cap: {n}");
    }

    #[test]
    fn limiter_reaches_the_cap_from_tick_quantized_oversampled_capture() {
        // Capture asks for twice the cap; Windows rounds the pacing sleep up to
        // whole 15.625 ms ticks: 33.3 ms -> 46.875 ms, 16.7 ms -> 31.25 ms.
        let n = admitted(15, 1000.0 / 46.875, 0, 10);
        assert!((148..=151).contains(&n), "text: {n} frames in 10 s");
        let n = admitted(30, 1000.0 / 31.25, 0, 10);
        assert!((297..=301).contains(&n), "motion: {n} frames in 10 s");
        // Without oversampling the cap is unreachable (66.7 ms -> 78.125 ms).
        let n = admitted(15, 1000.0 / 78.125, 0, 10);
        assert!(n <= 128, "{n}");
    }

    #[test]
    fn limiter_restarts_after_a_pause() {
        let mut limiter = RateLimiter::new(15);
        assert!(limiter.admit(0));
        assert!(limiter.admit(66_700));
        // A five second gap (minimized window, static VFR source)...
        assert!(limiter.admit(5_066_700));
        // ...then frames resume on a new phase.
        assert!(!limiter.admit(5_100_000));
        assert!(limiter.admit(5_133_400));
        limiter.set_fps(30);
        assert!(limiter.admit(5_140_000), "a new cap admits the next frame");
    }

    #[test]
    fn keyframe_requests_coalesce_and_fire_on_the_trailing_edge() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut gate = KeyframeGate::new(KEYFRAME_MIN_INTERVAL);
        assert!(!gate.poll(ms(0)), "nothing requested");
        gate.request();
        assert!(gate.poll(ms(0)), "first request fires at once");
        // Three requests inside the window collapse into one, held not lost.
        gate.request();
        assert!(!gate.poll(ms(100)));
        gate.request();
        assert!(!gate.poll(ms(300)));
        gate.request();
        assert!(!gate.poll(ms(499)));
        assert!(gate.is_pending());
        assert!(gate.poll(ms(500)), "fires when the window expires");
        assert!(!gate.poll(ms(560)), "and only once");
        assert!(!gate.is_pending());
    }

    #[test]
    fn a_keyframe_on_schedule_satisfies_pending_requests() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut gate = KeyframeGate::new(KEYFRAME_MIN_INTERVAL);
        gate.request();
        assert!(gate.poll(ms(0)));
        gate.request();
        gate.on_keyframe(ms(200));
        assert!(!gate.is_pending());
        assert!(!gate.poll(ms(800)), "satisfied, nothing more to force");
        // A request right after a scheduled keyframe waits out the window
        // measured from that keyframe.
        gate.on_keyframe(ms(1_000));
        gate.request();
        assert!(!gate.poll(ms(1_100)));
        assert!(!gate.poll(ms(1_499)));
        assert!(gate.poll(ms(1_500)));
    }

    #[test]
    fn a_dropped_keyframe_keeps_the_request_alive() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut gate = KeyframeGate::new(KEYFRAME_MIN_INTERVAL);
        // The loop records a produced keyframe, then re-requests when the send fails.
        gate.on_keyframe(ms(0));
        gate.request();
        assert!(!gate.poll(ms(100)), "rate limited from the dropped keyframe");
        assert!(gate.poll(ms(500)));
    }

    #[test]
    fn meter_reports_fps_and_bitrate_over_its_window() {
        let t0 = Instant::now();
        let mut meter = Meter::new(Duration::from_secs(2), t0);
        // 15 fps of 10 kB frames for 4 seconds = 15 fps, 1.2 Mbps.
        for i in 0..60u64 {
            meter.record(t0 + Duration::from_micros(i * 66_667), 10_000);
        }
        let (fps, bps) = meter.rates(t0 + Duration::from_secs(4));
        assert!((14.0..=16.0).contains(&fps), "fps {fps}");
        assert!((1_100_000..=1_300_000).contains(&bps), "bps {bps}");
        // A paused source decays to zero instead of reporting stale rates.
        let (fps, bps) = meter.rates(t0 + Duration::from_secs(10));
        assert_eq!((fps, bps), (0.0, 0));
    }

    #[test]
    fn meter_does_not_spike_at_start() {
        let t0 = Instant::now();
        let mut meter = Meter::new(Duration::from_secs(2), t0);
        meter.record(t0, 50_000);
        let (fps, _) = meter.rates(t0 + Duration::from_millis(1));
        assert!(fps <= 2.0, "fps {fps}");
    }

    #[test]
    fn timeline_stays_monotonic_across_reopens() {
        let t0 = Instant::now();
        let mut tl = Timeline::new(t0);
        tl.rebase(t0);
        assert_eq!(tl.map(10), 10);
        assert_eq!(tl.map(66_000), 66_000);
        // The stream reopens 2 s in; its own clock restarts near zero.
        tl.rebase(t0 + Duration::from_secs(2));
        assert_eq!(tl.map(500), 2_000_500);
        // A timestamp that would rewind is pushed just past the last one.
        tl.rebase(t0 + Duration::from_secs(1));
        assert_eq!(tl.map(0), 2_000_501);
        assert_eq!(tl.map(0), 2_000_502);
    }

    #[test]
    fn ninety_khz_conversion_and_wrap() {
        assert_eq!(to_90khz(0), 0);
        assert_eq!(to_90khz(1_000_000), 90_000);
        assert_eq!(to_90khz(66_667), 6_000);
        assert_eq!(to_90khz(11), 0);
        // 2^32 ticks at 90 kHz is 47721.858 s; just past it wraps to a small value.
        let wrap_us = (1u64 << 32) * 100 / 9 + 1;
        assert!(to_90khz(wrap_us) < 10, "{}", to_90khz(wrap_us));
        let before = to_90khz(wrap_us - 1_000_000);
        assert!(before > u32::MAX - 100_000, "{before}");
    }
}
